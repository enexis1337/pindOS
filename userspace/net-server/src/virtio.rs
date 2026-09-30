use core::sync::atomic::{fence, Ordering};
use alloc::alloc::{alloc_zeroed, Layout};

const QUEUE_SIZE: usize = 256;
/// Размер одного буфера приёма ( jumbo Ethernet-кадр с запасом ).
const BUF_SIZE: usize = 2048;
/// Сколько буферов публикуем в RX-очередь при инициализации.
const BUF_COUNT: usize = 32;

// Регистры legacy virtio-pci (MMIO BAR0, как у QEMU по умолчанию).
const VIRTIO_MMIO_STATUS:  u16 = 0x100;
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER:     u32 = 2;
const STATUS_DRIVER_OK:  u32 = 4;

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
    next_post: u16,
    /// Размер очереди, прочитанный у устройства (QUEUE_SIZE, io_base+12).
    qsize: usize,
    /// Физические адреса RX-буферов по индексу дескриптора (0 = не выделен).
    buf_phys: [u64; QUEUE_SIZE],
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
    outl(io_base + VIRTIO_MMIO_STATUS, 0);
    outl(io_base + VIRTIO_MMIO_STATUS, STATUS_ACKNOWLEDGE);
    outl(io_base + VIRTIO_MMIO_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    // Никаких фич не запрашиваем: legacy virtio-net без VIRTIO_F_VERSION_1.
    dbg_str("[virtio] driver init done\n");
}

/// Выставляет DRIVER_OK: очереди сконфигурированы и буферы опубликованы.
pub unsafe fn finish_device(io_base: u16) {
    outl(io_base + VIRTIO_MMIO_STATUS,
         STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
    dbg_str("[virtio] DRIVER_OK\n");
}

impl Virtqueue {
    pub unsafe fn init(io_base: u16, queue_idx: u16) -> Self {
        dbg_str("[virtio] init start\n");

        // Сбрасывать статус здесь нельзя: init() вызывается для каждой очереди,
        // и второй вызов обнулил бы статус, сбросив конфигурацию первой.
        // Handshake делает start_device() один раз, до настройки очередей.

        dbg_str("[virtio] QUEUE_SEL\n");
        outw(io_base + 14, queue_idx);

        // Размер очереди читаем у устройства, а не берём константой: QEMU
        // сообщает фактический QUEUE_SIZE, и жёсткое 256 могло не совпасть.
        let qsize = inw(io_base + 12) as usize;
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

        for i in 0..qsize - 1 {
            (*desc.add(i)).next  = (i + 1) as u16;
            (*desc.add(i)).flags = 1;
        }

        dbg_str("[virtio] QUEUE_PFN\n");
        // В QUEUE_PFN уходит ФИЗИЧЕСКИЙ адрес, не виртуальный.
        outl(io_base + 8,  (ptr_phys as u64 / 4096) as u32);

        dbg_str("[virtio] init done\n");

        let mut q = Self { desc, avail, used, free_head: 0, last_used: 0, io_base, queue_idx, next_post: 0, qsize, buf_phys: [0u64; QUEUE_SIZE] };
        // Для RX-очереди дескрипторы должны быть опубликованы в avail ring:
        // иначе у устройства нет ни одного буфера, куда писать, used.idx
        // никогда не растёт и recv() всегда возвращает None.
        q.post_receive_buffers(BUF_COUNT);
        q
    }

    /// Публикует буферы приёма: каждому дескриптору выдаётся участок памяти,
    /// дескриптор кладётся в avail ring.
    ///
    /// Идём по индексам, а не по free-list: в этой реализации 0 служит и
    /// началом списка, и его концом, поэтому обход по `next` неотличим от
    /// пустого списка.
    unsafe fn post_receive_buffers(&mut self, count: usize) {
        let mut posted = 0usize;
        while posted < count {
            let idx = self.next_post as usize;
            if idx >= self.qsize - 1 { break; }
            self.next_post += 1;

            // Буфер приёма — тоже физическая память из ядра: virtio пишет
            // кадр напрямую по физическому адресу из дескриптора.
            let (buf_virt, buf_phys) = dma_alloc_pages((BUF_SIZE + 4095) / 4096);
            if buf_virt == 0 { break; }

            // Физический адрес буфера запоминаем по индексу дескриптора:
            // в recv() нам нужно копировать данные именно из него.
            self.buf_phys[idx] = buf_phys;

            (*self.desc.add(idx)).addr  = buf_phys;
            (*self.desc.add(idx)).len   = BUF_SIZE as u32;
            (*self.desc.add(idx)).flags = 0; // WRITE: устройство пишет в буфер
            (*self.desc.add(idx)).next  = 0;

            let avail_idx = (*self.avail).idx as usize % self.qsize;
            (*self.avail).ring[avail_idx] = idx as u16;
            fence(Ordering::Release);
            (*self.avail).idx = (*self.avail).idx.wrapping_add(1);
            posted += 1;
        }
        fence(Ordering::Release);
        if posted > 0 {
            dbg_str("[virtio] rx buffers ready\n");
            // Сообщаем устройству, что буферы появились.
            outw(self.io_base + 16, self.queue_idx);
        }
    }

    pub unsafe fn send(&mut self, data: &[u8]) {
        // Данные копируем в DMA-буфер: дескриптору отдаётся физический адрес.
        let (dvirt, dphys) = dma_alloc_pages((data.len() + 4095) / 4096);
        if dvirt == 0 { return; }
        core::ptr::copy_nonoverlapping(data.as_ptr(), dvirt as *mut u8, data.len());

        let idx = self.free_head as usize;
        self.free_head = (*self.desc.add(idx)).next;

        (*self.desc.add(idx)).addr  = dphys;
        (*self.desc.add(idx)).len   = data.len() as u32;
        (*self.desc.add(idx)).flags = 0;

        let avail_idx = (*self.avail).idx as usize % self.qsize;
        (*self.avail).ring[avail_idx] = idx as u16;
        fence(Ordering::Release);
        (*self.avail).idx = (*self.avail).idx.wrapping_add(1);
        fence(Ordering::Release);

        outw(self.io_base + 16, self.queue_idx);
    }

    pub unsafe fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        if (*self.used).idx == self.last_used { return None; }

        let elem = &(*self.used).ring[self.last_used as usize % self.qsize];
        let did = elem.id as usize;
        let phys = self.buf_phys[did];
        if phys == 0 { return None; }
        // Данные лежат в DMA-буфере, доступном нам как userspace-виртуальная
        // память по тому же адресу (ядро замапило phys по выданному virt).
        let len  = (elem.len as usize).min(buf.len());

        core::ptr::copy_nonoverlapping(phys as *const u8, buf.as_mut_ptr(), len);
        self.last_used = self.last_used.wrapping_add(1);

        (*self.desc.add(did)).next = self.free_head;
        self.free_head = elem.id as u16;

        // Буфер надо вернуть устройству: без этого RX-очередь опустеет и
        // через несколько пакетов перестанет получать что-либо.
        self.post_receive_buffers(1);

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

unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    core::arch::asm!("in ax, dx", out("ax") v, in("dx") port, options(nostack));
    v
}

unsafe fn outw(port: u16, val: u16) {
    core::arch::asm!("out dx, ax", in("dx") port, in("ax") val, options(nostack));
}
unsafe fn outl(port: u16, val: u32) {
    core::arch::asm!("out dx, eax", in("dx") port, in("eax") val, options(nostack));
}
