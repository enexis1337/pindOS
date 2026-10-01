use core::sync::atomic::{fence, Ordering};
use alloc::alloc::{alloc_zeroed, Layout};

const QUEUE_SIZE: usize = 256;
/// Размер одного буфера приёма ( jumbo Ethernet-кадр с запасом ).
/// Размер virtio_net_hdr для legacy virtio-net без VIRTIO_NET_F_MRG_RXBUF:
/// flags(1) + gso_type(1) + hdr_len(2) + gso_size(2) + csum_start(2) +
/// csum_offset(2) = 10 байт. Устройство пишет этот заголовок в начало каждого
/// принятого буфера, поэтому smoltcp должен получить данные ПОСЛЕ него.
pub const VIRTIO_NET_HDR_LEN: usize = 10;

/// Максимальный Ethernet-кадр, который нам нужно принять.
pub const MAX_FRAME: usize = 1514;

/// RX-буфер обязан вмещать заголовок плюс кадр.
pub const BUF_SIZE: usize = VIRTIO_NET_HDR_LEN + MAX_FRAME;
/// Сколько буферов публикуем в RX-очередь при инициализации.
const BUF_COUNT: usize = 32;

// Карта регистров legacy virtio-pci (I/O BAR0). Это НЕ MMIO-карта virtio-mmio:
// у legacy-pci смещения маленькие, а Status — однобайтовый регистр.
// Смешивание mmio (0x100) и legacy (0x12) было причиной мусорных readback.
const REG_DEVICE_FEATURES_LO: u16 = 0x00; // 32-бит, младшие фичи
const REG_DEVICE_FEATURES_HI: u16 = 0x04; // 32-бит, старшие фичи
const REG_DRIVER_FEATURES: u16 = 0x08; // 32-бит
const REG_QUEUE_PFN:      u16 = 0x08; // 32-бит
const REG_QUEUE_NUM:      u16 = 0x0C; // 16-бит!
const REG_QUEUE_SEL:      u16 = 0x0E; // 16-бит
const REG_QUEUE_NOTIFY:   u16 = 0x10; // 16-бит
const REG_STATUS:         u16 = 0x12; // 8-бит!
const REG_ISR:            u16 = 0x13; // 8-бит, Interrupt Status (write-1-to-clear)
const REG_MAC:            u16 = 0x14; // 6 байт, читаем по одному

/// VIRTIO_NET_F_MAC (бит 5) — устройство сообщает MAC в конфигурации.
const VIRTIO_NET_F_MAC: u32 = 1 << 5;

// Флаги дескриптора vring. Значения заданы спецификацией virtio:
// NEXT = 1, WRITE = 2. Раньше для RX стояло flags = 1, то есть NEXT, а не
// WRITE: устройство пыталось идти по цепочке и падало с "Looped descriptor".
const VRING_DESC_F_NEXT:  u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;
const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER:     u8 = 2;
const STATUS_DRIVER_OK:  u8 = 4;

#[repr(C)]
struct VirtqDesc {
    addr:  u64,
    len:   u32,
    flags: u16,
    next:  u16,
}

#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx:   u16,
    ring:  [u16; QUEUE_SIZE],
}

#[repr(C)]
struct VirtqUsedElem { id: u32, len: u32 }

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx:   u16,
    ring:  [VirtqUsedElem; QUEUE_SIZE],
}

pub struct Virtqueue {
    desc:      *mut VirtqDesc,
    avail:     *mut VirtqAvail,
    used:      *mut VirtqUsed,
    free_head: u16,
    last_used: u16,
    io_base:   u16,
    queue_idx: u16,
    /// Следующий дескриптор для публикации в avail ring (RX).
    /// Размер очереди, прочитанный у устройства (QUEUE_SIZE, io_base+12).
    qsize: usize,
    /// Физические адреса RX-буферов по индексу дескриптора (0 = не выделен).
    buf_phys: [u64; QUEUE_SIZE],
    /// Следующий свободный дескриптор для TX.
    next_tx: u16,
    /// Сколько элементов used-кольца TX-очереди мы уже забрали.
    last_tx: u16,
}

unsafe fn dbg_outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nostack));
}
unsafe fn dbg_str(s: &str) {
    for &b in s.as_bytes() {
        dbg_outb(0x3f8, b);
    }
}

/// Полный handshake с virtio-устройством. Вызывается ровно один раз, до
/// настройки очередей и после неё ставит DRIVER_OK.
///
/// Без DRIVER_OK устройство остаётся в DRIVER_FAILS: TX-кольцо ещё кое-как
/// работает, но RX не приходит никогда и used.idx не растёт.
pub unsafe fn start_device(io_base: u16) {
    // ── Шаг 3: проверка пути ввода-вывода ДО handshake ──
    // Legacy DeviceID по +0x00 = 0x554d4551 ("QEMU"). MAC лежит шестью
    // байтами по +0x14..+0x19. Если MAC читается верно, порты и BAR
    // заведомо рабочие, и дальше можно верить остальным readback.
    let feat_lo0 = inl(io_base + REG_DEVICE_FEATURES_LO);
    dbg_str(&alloc::format!("[virtio] host features lo = {:#x}\n", feat_lo0));
    let m0 = inb(io_base + REG_MAC + 0);
    let m1 = inb(io_base + REG_MAC + 1);
    let m2 = inb(io_base + REG_MAC + 2);
    let m3 = inb(io_base + REG_MAC + 3);
    let m4 = inb(io_base + REG_MAC + 4);
    let m5 = inb(io_base + REG_MAC + 5);
    dbg_str(&alloc::format!("[virtio] MAC = {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
        m0, m1, m2, m3, m4, m5));

    // ── Шаг 4: handshake строго по порядку, 8-битный Status ──
    outb(io_base + REG_STATUS, 0);
    let s = inb(io_base + REG_STATUS);
    dbg_str(&alloc::format!("[virtio] status after reset      = {:#04x} (want 0x00)\n", s));

    outb(io_base + REG_STATUS, STATUS_ACKNOWLEDGE);
    let s = inb(io_base + REG_STATUS);
    dbg_str(&alloc::format!("[virtio] status after ACKNOWLEDGE = {:#04x} (want 0x01)\n", s));

    outb(io_base + REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    let s = inb(io_base + REG_STATUS);
    dbg_str(&alloc::format!("[virtio] status after DRIVER     = {:#04x} (want 0x03)\n", s));

    // Фичи устройства 64-битные: младшие 32 по +0x00, старшие 32 по +0x04.
    // Раньше читалось только +0x04, то есть старшая половина (у legacy-устройства
    // она 0), и мы писали в GuestFeatures нули, не согласуя даже VIRTIO_NET_F_MAC.
    // GuestFeatures пишется по +0x08.
    let feat_lo = inl(io_base + REG_DEVICE_FEATURES_LO);
    let feat_hi = inl(io_base + REG_DEVICE_FEATURES_HI);

    // Оставляем только то, что реально поддерживаем: VIRTIO_NET_F_MAC.
    // VERSION_1 (бит 32, в старшей половине) нам недоступен и не нужен —
    // драйвер говорит на legacy-раскладке колец.
    let mut driver_features = 0u32;
    if feat_lo & VIRTIO_NET_F_MAC != 0 {
        driver_features |= VIRTIO_NET_F_MAC;
    }
    outl(io_base + REG_DRIVER_FEATURES, driver_features);
    dbg_str(&alloc::format!("[virtio] features lo={:#x} hi={:#x} drv={:#x} (has MAC={})\n",
        feat_lo, feat_hi, driver_features, feat_lo & VIRTIO_NET_F_MAC != 0));

    dbg_str("[virtio] driver init done\n");
}

/// Выставляет DRIVER_OK: очереди сконфигурированы и буферы опубликованы.
pub unsafe fn finish_device(io_base: u16) {
    outb(io_base + REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
    let s = inb(io_base + REG_STATUS);
    dbg_str(&alloc::format!("[virtio] status after DRIVER_OK  = {:#04x} (want 0x07)\n", s));
    dbg_str("[virtio] DRIVER_OK\n");
}

/// Публикует RX-буферы и отправляет notify — вызывается ПОСЛЕ DRIVER_OK.
///
/// Порядок важен: пока статус не DRIVER_OK, устройство игнорирует notify, и
/// пакеты, пришедшие раньше, остаются отложенными в SLIRP без надежды на
/// доставку. Notify после DRIVER_OK заставляет QEMU переотдать отложенное.
pub unsafe fn arm_rx_buffers(rx: &mut Virtqueue) {
    rx.post_receive_buffers(BUF_COUNT);
    outw(rx.io_base + REG_QUEUE_NOTIFY, 0);
    dbg_str(&alloc::format!(
        "[virtio] RX armed: avail.idx={} notify(0) sent, isr={:#x}\n",
        (*rx.avail).idx, inb(rx.io_base + REG_ISR)));
}

impl Virtqueue {
    pub unsafe fn init(io_base: u16, queue_idx: u16) -> Self {
        dbg_str("[virtio] init start\n");

        // Сбрасывать статус здесь нельзя: init() вызывается для каждой очереди,
        // и второй вызов обнулил бы статус, сбросив конфигурацию первой.
        // Handshake делает start_device() один раз, до настройки очередей.

        dbg_str("[virtio] QUEUE_SEL\n");
        outw(io_base + REG_QUEUE_SEL, queue_idx);

        // Размер очереди читаем у устройства, а не берём константой: QEMU
        // сообщает фактический QUEUE_SIZE, и жёсткое 256 могло не совпасть.
        let qsize = inw(io_base + REG_QUEUE_NUM) as usize;
        let qsize = if qsize == 0 { QUEUE_SIZE } else { qsize };
        dbg_str(&alloc::format!("[virtio] queue {} size={} (const {})\n", queue_idx, qsize, QUEUE_SIZE));

        // Legacy-раскладка: desc, затем avail, затем used со смещения,
        // округлённого вверх до 4096. Раньше used стоял сразу за avail без
        // выравнивания, из-за чего устройство читало кольцо used по
        // неверному адресу и ничего не доставляло.
        let desc_bytes  = core::mem::size_of::<VirtqDesc>() * qsize;
        let avail_bytes = core::mem::size_of::<VirtqAvail>();
        let used_bytes  = core::mem::size_of::<VirtqUsed>();

        let used_offset = align_up(desc_bytes + avail_bytes, 4096);
        let total = used_offset + used_bytes;
        dbg_str(&alloc::format!("[virtio] layout: desc={} avail={} used_off={:#x} total={}\n",
            desc_bytes, avail_bytes, used_offset, total));

        dbg_str("[virtio] dma_alloc\n");

        // Кольца очереди размещаются в физически непрерывной памяти, выданной
        // ядром: userspace-виртуальные адреса устройство использовать не может.
        let (ptr_virt, ptr_phys) = dma_alloc_pages((total + 4095) / 4096);

        let ptr = ptr_virt as *mut u8;
        dbg_str(&alloc::format!("[virtio] queue mem virt={:#x} phys={:#x}\n", ptr_virt, ptr_phys));
        if ptr_virt == 0 || ptr_phys == 0 { panic!("dma_alloc for queue failed"); }

        let desc  = ptr as *mut VirtqDesc;
        let avail = ptr.add(desc_bytes) as *mut VirtqAvail;
        let used  = ptr.add(used_offset) as *mut VirtqUsed;

        // DMA-память приходит из buddy грязной: без обнуления avail.idx и
        // used.idx содержат мусор, и устройство читает несуществующие дескрипторы.
        for i in 0..total {
            core::ptr::write_volatile(ptr.add(i), 0u8);
        }
        fence(Ordering::Release);

        // НЕ связываем дескрипторы в цепочку (flags=1 / NEXT). Так делают для
        // scatter-gather, но здесь каждый буфер самостоятельный, и устройство,
        // следуя NEXT, уходило в неинициализированные дескрипторы — QEMU
        // отвечал "bogus descriptor or out of resources" и
        // "receive queue contains no in buffers".
        // Для TX свободные дескрипторы берутся из free_head, который
        // инициализируется отдельно в send().
        for i in 0..qsize {
            (*desc.add(i)).next  = 0;
            (*desc.add(i)).flags = 0;
            (*desc.add(i)).addr  = 0;
            (*desc.add(i)).len   = 0;
        }

        dbg_str("[virtio] QUEUE_PFN\n");
        // В QUEUE_PFN уходит ФИЗИЧЕСКИЙ адрес, не виртуальный.
        outl(io_base + REG_QUEUE_PFN, (ptr_phys as u64 / 4096) as u32);

        // Диагностика: читаем QUEUE_PFN и QUEUE_NUM back. 0xffffffff означает
        // плавающую шину, то есть устройство по этому BAR не отвечает вовсе.
        let pfn_rb = inl(io_base + REG_QUEUE_PFN);
        let num_rb = inw(io_base + REG_QUEUE_NUM);
        dbg_str(&alloc::format!("[virtio] q{} readback PFN={:#x} (wrote {:#x}) NUM={}\n",
            queue_idx, pfn_rb, (ptr_phys as u64 / 4096) as u32, num_rb));

        dbg_str("[virtio] init done\n");

        let mut q = Self { desc, avail, used, free_head: 0, last_used: 0, io_base, queue_idx, qsize, buf_phys: [0u64; QUEUE_SIZE], next_tx: 0, last_tx: 0 };
        // Для RX-очереди дескрипторы должны быть опубликованы в avail ring:
        // иначе у устройства нет ни одного буфера, куда писать, used.idx
        // никогда не растёт и recv() всегда возвращает None.
        // Буферы приёма публикуются только в очереди 0. У TX-очереди их быть
        // не должно: лишние WRITE-дескрипторы там бессмысленны.
        if queue_idx == 0 {
            // Буферы публикуются, но notify здесь НЕ делаем: статус ещё не
            // DRIVER_OK, и устройство отбрасывает notify вместе с уже
            // пришедшими пакетами. Notify уходит после DRIVER_OK, в arm_rx_buffers().
        }
        // Диагностика: что реально лежит в кольцах сразу после инициализации.
        dbg_str(&alloc::format!("[virtio] q{} memcheck avail@{:p} used@{:p} avail.idx={} avail.ring[0]={} used.idx={}\n",
            queue_idx, q.avail, q.used, (*q.avail).idx, (*q.avail).ring[0], (*q.used).idx));
        q
    }

    /// Публикует буферы приёма: каждому дескриптору выдаётся участок памяти,
    /// дескриптор кладётся в avail ring.
    ///
    /// Идём по индексам, а не по free-list: в этой реализации 0 служит и
    /// началом списка, и его концом, поэтому обход по `next` неотличим от
    /// пустого списка.
    /// Первоначальная публикация RX-буферов: выделяем буферы для дескрипторов
    /// 0..count. Дальше они возвращаются в avail через repost_descriptor() по
    /// индексу из used-элемента, поэтому отдельного счётчика не требуется.
    pub unsafe fn post_receive_buffers(&mut self, count: usize) {
        let n = if count > self.qsize { self.qsize } else { count };
        let mut posted = 0usize;
        for idx in 0..n {
            // Буфер приёма — физическая память из ядра: устройство пишет кадр
            // напрямую по физическому адресу из дескриптора.
            let (_buf_virt, buf_phys) = dma_alloc_pages((BUF_SIZE + 4095) / 4096);
            if buf_phys == 0 { break; }
            self.buf_phys[idx] = buf_phys;
            self.publish(idx, buf_phys);
            posted += 1;
        }
        if posted > 0 {
            dbg_str("[virtio] rx buffers ready\n");
        }
    }

    /// Единая точка публикации RX-дескриптора.
    ///
    /// avail.idx растёт монотонно и монотонно же используется как указатель в
    /// кольцо: индекс слота — `avail.idx % qsize`. Счётчик дескрипторов не
    /// нужен, освободившийся индекс берётся из used-элемента.
    pub unsafe fn publish(&mut self, did: usize, phys: u64) {
        (*self.desc.add(did)).addr  = phys;
        (*self.desc.add(did)).len   = BUF_SIZE as u32;
        (*self.desc.add(did)).flags = VRING_DESC_F_WRITE; // устройство пишет
        (*self.desc.add(did)).next  = 0;                  // без цепочки
        let avail_idx = (*self.avail).idx as usize % self.qsize;
        (*self.avail).ring[avail_idx] = did as u16;
        fence(Ordering::Release);
        (*self.avail).idx = (*self.avail).idx.wrapping_add(1);
        fence(Ordering::Release);
        outw(self.io_base + REG_QUEUE_NOTIFY, self.queue_idx);
    }

    /// Возвращает использованный дескриптор в очередь. Индекс уже взят из
    /// used-элемента в recv(), здесь только перепубликация с тем же буфером.
    pub unsafe fn repost_descriptor(&mut self, did: usize, phys: u64) {
        self.publish(did, phys);
    }

    /// Отправляет Ethernet-кадр. Данные уже без заголовков L2/L3/L4 —
    /// добавляется только virtio_net_hdr, который smoltcp не знает.
    pub unsafe fn send(&mut self, data: &[u8]) {
        let total = VIRTIO_NET_HDR_LEN + data.len();
        let (dvirt, dphys) = dma_alloc_pages((total + 4095) / 4096);
        if dvirt == 0 { return; }
        // Заголовок нулевой: без GSO и без пересчёта контрольной суммы
        // устройство берёт контрольные суммы из заголовков L4.
        let d = dvirt as *mut u8;
        for i in 0..VIRTIO_NET_HDR_LEN {
            core::ptr::write_volatile(d.add(i), 0u8);
        }
        core::ptr::copy_nonoverlapping(data.as_ptr(), d.add(VIRTIO_NET_HDR_LEN), data.len());

        // Свободный дескриптор для TX берём из used-элемента: устройство
        // вернуло его после обработки, значит он свободен. Раньше здесь был
        // монотонный next_tx, который исчерпывался после qsize отправок, и
        // send() молча терял кадры — из-за этого вставал и RX, просто потому
        // что трафика больше не было.
        let idx = match unsafe { self.pop_free_tx() } {
            Some(i) => i,
            None => {
                dbg_str("[virtio] TX: no free descriptors, send dropped\n");
                return;
            }
        };

        (*self.desc.add(idx)).addr  = dphys;
        (*self.desc.add(idx)).len   = total as u32;
        (*self.desc.add(idx)).flags = 0; // TX: устройство читает, WRITE не нужен

        let avail_idx = (*self.avail).idx as usize % self.qsize;
        (*self.avail).ring[avail_idx] = idx as u16;
        fence(Ordering::Release);
        (*self.avail).idx = (*self.avail).idx.wrapping_add(1);
        fence(Ordering::Release);

        outw(self.io_base + REG_QUEUE_NOTIFY, self.queue_idx);
    }

    /// Забирает свободный TX-дескриптор из used-кольца.
    ///
    /// used.idx монотонно указывает на элементы, которые устройство
    /// обработало; last_tx — сколько из них мы уже забрали. Элемент
    /// освободил дескриптор, значит его можно использовать снова.
    /// Забирает свободный TX-Дескриптор.
    ///
    /// next_tx — сколько дескрипторов отдано в кольцо, last_tx — сколько из них
    /// устройство вернуло через used. Значит «в полёте» ровно next_tx-last_tx
    /// дескрипторов, а свободно qsize минус это число. Индекс берём как
    /// next_tx % qsize, то есть кольцевой: раньше он рос монотонно и упирался
    /// в qsize, после чего send() молча терял все кадры.
    unsafe fn pop_free_tx(&mut self) -> Option<usize> {
        let used_idx = core::ptr::read_volatile(&(*self.used).idx);
        // Учитываем только возвраты, которые мы ещё не забрали.
        if last_after(self.last_tx, used_idx) {
            self.last_tx = used_idx;
        }
        let in_flight = (self.next_tx.wrapping_sub(self.last_tx)) as usize;
        if in_flight >= self.qsize {
            return None;
        }
        let idx = self.next_tx as usize % self.qsize;
        self.next_tx = self.next_tx.wrapping_add(1);
        Some(idx)
    }

    /// Периодическая диагностика состояния RX-кольца.
    pub unsafe fn dump_rx_state(&self, tag: &str) {
        fence(Ordering::Acquire);
        let a_idx = core::ptr::read_volatile(&(*self.avail).idx);
        let a_flg = core::ptr::read_volatile(&(*self.avail).flags);
        let u_idx = core::ptr::read_volatile(&(*self.used).idx);
        let u_flg = core::ptr::read_volatile(&(*self.used).flags);
        let u_id = core::ptr::read_volatile(&(*self.used).ring[0].id);
        let u_len = core::ptr::read_volatile(&(*self.used).ring[0].len);
        let d_addr = core::ptr::read_volatile(&(*self.desc).addr);
        let d_len = core::ptr::read_volatile(&(*self.desc).len);
        let d_flg = core::ptr::read_volatile(&(*self.desc).flags);
        let a0 = core::ptr::read_volatile(&(*self.avail).ring[0]);
        dbg_str(&alloc::format!("[virtio]   RAW used.ring[0]={{id:{} len:{}}} desc[0]={{addr:{:#x} len:{} flags:{:#x}}} avail.ring[0]={}\n",
            u_id, u_len, d_addr, d_len, d_flg, a0));
        // Status читаем отдельно: 0x47 означает NEEDS_RESET (бит 0x40),
        // то есть устройство само отказалось от нашего драйвера.
        let st = inb(self.io_base + REG_STATUS);
        dbg_str(&alloc::format!("[virtio] RX {}: status={:#04x} (NEEDS_RESET={})\n",
            tag, st, (st & 0x40) != 0));
        dbg_str(&alloc::format!(
            "[virtio] RX {}: avail.idx={} avail.flags={} used.idx={} used.flags={} last_used={} isr={:#x}\n",
            tag, a_idx, a_flg, u_idx, u_flg, self.last_used, inb(self.io_base + REG_ISR)));
    }

    pub unsafe fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        fence(Ordering::Acquire);
        let used_idx = core::ptr::read_volatile(&(*self.used).idx);
        if used_idx == self.last_used { return None; }

        // Диагностика: устройство наконец-то что-то отдало.
        dbg_str(&alloc::format!("[virtio] q{} used.idx={} last_used={}\n",
            self.queue_idx, (*self.used).idx, self.last_used));

        let elem = &(*self.used).ring[self.last_used as usize % self.qsize];
        let did = elem.id as usize;
        let phys = self.buf_phys[did];
        if phys == 0 { return None; }

        // Устройство кладёт в буфер virtio_net_hdr (10 байт) перед кадром.
        // smoltcp о нём не знает, поэтому пропускаем и отдаём только Ethernet.
        let raw = elem.len as usize;
        if raw <= VIRTIO_NET_HDR_LEN { return None; }
        let frame = &*(phys as *const u8).add(VIRTIO_NET_HDR_LEN);
        let len = (raw - VIRTIO_NET_HDR_LEN).min(buf.len());

        core::ptr::copy_nonoverlapping(frame, buf.as_mut_ptr(), len);
        self.last_used = self.last_used.wrapping_add(1);

        // Буфер возвращаем ТЕМ ЖЕ путём, что и при инициализации: через
        // repost_descriptor. Иначе через BUF_COUNT кадров приём встал бы.
        self.repost_descriptor(did, phys);

        Some(len)
    }
}

/// Выделяет через ядро физически непрерывный блок в `pages` страниц.
/// Возвращает (virt, phys); (0, 0) при ошибке.
///
/// Ядро мапит физические фреймы в наше адресное пространство, поэтому по
/// virt-адресу память тоже доступна — но в дескрипторы идёт phys.
///
/// TODO(capabilities): выдача DMA-памяти должна проверять capability
/// `DmaMemory`; сейчас проверки в ядре нет (см. syscall::sys_dma_alloc).
pub fn dma_alloc_pages(pages: usize) -> (u64, u64) {
    // out-буфер для пары (virt, phys) кладём на стек.
    let mut out = [0u64; 2];
    let rc: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 5u64,
            in("rdi") pages as u64,
            in("rsi") out.as_mut_ptr() as u64,
            lateout("rax") rc,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    if rc != 0 {
        unsafe { dbg_str("[virtio] dma_alloc FAILED\n") };
        return (0, 0);
    }
    (out[0], out[1])
}

/// Округляет `v` вверх до кратного `align` (степень двойки).
const fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

/// true, если `a` — более позднее значение счётчика, чем `b`, с учётом
/// переполнения u16. Счётчики монотонны и переворачиваются через 65535.
const fn last_after(a: u16, b: u16) -> bool {
    a != b && (a.wrapping_sub(b) as i16) < 0
}

unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nostack));
    v
}

unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nostack));
}

unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    core::arch::asm!("in ax, dx", out("ax") v, in("dx") port, options(nostack));
    v
}

unsafe fn inl(port: u16) -> u32 {
    let v: u32;
    core::arch::asm!("in eax, dx", out("eax") v, in("dx") port, options(nostack));
    v
}

unsafe fn outw(port: u16, val: u16) {
    core::arch::asm!("out dx, ax", in("dx") port, in("ax") val, options(nostack));
}
unsafe fn outl(port: u16, val: u32) {
    core::arch::asm!("out dx, eax", in("dx") port, in("eax") val, options(nostack));
}
