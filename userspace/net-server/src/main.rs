#![no_std]
#![no_main]

//! TCP/IP стек полностью в userspace
//! Ядро предоставляет только raw доступ к NIC через capability
//!
//! Использует smoltcp для TCP/IP обработки
//! Virtio-net для QEMU/KVM

extern crate alloc;

#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    for i in 0..n { core::ptr::write_volatile(dest.add(i), core::ptr::read_volatile(src.add(i))); }
    dest
}
#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut u8, c: i32, n: usize) -> *mut u8 {
    for i in 0..n { core::ptr::write_volatile(s.add(i), c as u8); }
    s
}
#[no_mangle]
pub unsafe extern "C" fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let a = core::ptr::read_volatile(s1.add(i));
        let b = core::ptr::read_volatile(s2.add(i));
        if a != b { return a as i32 - b as i32; }
    }
    0
}
#[no_mangle]
pub unsafe extern "C" fn bcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    memcmp(s1, s2, n)
}

/// Print via syscall write(1, ...)
macro_rules! println {
    ($($arg:tt)*) => {{
        let _fmt = core::format_args!($($arg)*);
        // Estimate formatted length: use a generous upper bound
        let _cap = 256usize;
        let _layout = core::alloc::Layout::array::<u8>(_cap).unwrap();
        let _ptr = unsafe { alloc::alloc::alloc(_layout) };
        if !_ptr.is_null() {
            struct HeapWriter(*mut u8, usize, usize);
            impl core::fmt::Write for HeapWriter {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                    let b = s.as_bytes();
                    let remaining = self.2 - self.1;
                    if b.len() > remaining { return Err(core::fmt::Error); }
                    for i in 0..b.len() {
                        unsafe { core::ptr::write_volatile(self.0.add(self.1 + i), b[i]); }
                    }
                    self.1 += b.len();
                    Ok(())
                }
            }
            let mut _w = HeapWriter(_ptr, 0, _cap);
            let _ = core::fmt::Write::write_fmt(&mut _w, _fmt);
            if _w.1 < _cap {
                unsafe { core::ptr::write_volatile(_ptr.add(_w.1), b'\n'); }
                _w.1 += 1;
                #[allow(unused_unsafe)]
                unsafe {
                    core::arch::asm!(
                        "syscall",
                        in("rax") 1u64,
                        in("rdi") 1u64,
                        in("rsi") _ptr,
                        in("rdx") _w.1,
                    );
                }
            }
        }
    }};
}

mod alloc_impl;
mod device;
mod pci;
mod virtio;

use alloc::vec;
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    socket::{udp, icmp},
    time::Instant,
    wire::{EthernetAddress, IpAddress, IpCidr, Icmpv4Packet, Icmpv4Repr, Ipv4Address},
};

/// Завершение процесса через sys_exit(60).
///
/// Никогда не возвращает: ядро в `sys_exit` не даёт вернуться в userspace.
fn sys_exit(code: i32) -> ! {
    unsafe {
        core::arch::asm!("syscall", in("rax") 60u64, in("rdi") code as u64, options(nostack));
    }
    // Страховка: если ядро всё же вернуло управление, крутимся тут, а не
    // продолжаем работу в userspace с уже освобождённым состоянием.
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_exit(main())
}

/// Код возврата попадёт в sys_exit, а dealduck увидит его через sys_waitpid.
fn main() -> i32 {
    println!("[net-server] starting virtio-net driver...");

    // 1. Найти virtio-net устройство на PCI шине
    let pci_dev = match pci::find_virtio_net() {
        Some(dev) => {
            println!("[net-server] found virtio-net at PCI bus={} slot={}", dev.bus, dev.slot);
            dev
        }
        None => {
            println!("[net-server] ERROR: virtio-net device not found!");
            return 1;
        }
    };

    // 2. Инициализировать RX и TX очереди
    // Handshake с устройством — один раз, до настройки очередей.
    unsafe { virtio::start_device(pci_dev.bar0) };

    dbg_str("[main] init rx_queue\n");
    let mut rx_queue = unsafe { virtio::Virtqueue::init(pci_dev.bar0, 0) };
    dbg_str("[main] init tx_queue\n");
    let tx_queue = unsafe { virtio::Virtqueue::init(pci_dev.bar0, 1) };
    dbg_str("[main] queues done\n");
    // Обе очереди готовы и буферы опубликованы — можно разрешить работу.
    unsafe { virtio::finish_device(pci_dev.bar0) };
    // Буферы и notify — строго после DRIVER_OK.
    unsafe { virtio::arm_rx_buffers(&mut rx_queue) };

    // 3. Создать Device wrapper для smoltcp
    let mut device = device::VirtioNetDevice {
        rx_queue,
        tx_queue,
        rx_buf: [0u8; 1514],
        arp_req_tx: 0,
        arp_reply_rx: 0,
        arp_request_rx: 0,
        frames_rx: 0,
        frames_tx: 0,
    };
    dbg_str("[main] device created\n");

    // 4. Проверка: маленькая аллокация (убедимся что allocator жив)
    dbg_str("[main] test alloc\n");
    {
        let v = alloc::vec::Vec::<u8>::with_capacity(64);
        dbg_str("[main] vec created, len=");
        let n = v.len() as u64;
        for shift in (0..16).step_by(4).rev() {
            let nibble = (n >> shift) & 0xf;
            let c = if nibble < 10 { b'0' + nibble as u8 } else { b'a' + nibble as u8 - 10 };
            dbg_outb(c);
        }
        dbg_outb(b'\n');
    }

    // 5. Настроить smoltcp интерфейс
    println!("[net-server] configuring smoltcp...");
    let mac = EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    let config = Config::new(mac.into());
    let mut iface = Interface::new(config, &mut device, Instant::ZERO);

    // Адреса QEMU user-net (SLIRP): гость 10.0.2.15/24, шлюз 10.0.2.2.
    // Прежний адрес 10.0.2.2 совпадал с адресом шлюза, поэтому ARP-запрос
    // уходил самому себе и ответа не было.
    let guest_ip = Ipv4Address::new(10, 0, 2, 15);
    let gateway  = Ipv4Address::new(10, 0, 2, 2);
    iface.update_ip_addrs(|addr_list| {
        addr_list.push(IpCidr::new(guest_ip.into(), 24)).ok();
    });
    match iface.routes_mut().add_default_ipv4_route(gateway) {
        Ok(_) => println!("[net-server] default route via {}", gateway),
        Err(e) => println!("[net-server] ERROR: cannot add default route: {:?}", e),
    }
    println!("[net-server] ip={}/24 gw={}", guest_ip, gateway);

    let udp_rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 2048]);
    let udp_tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 2048]);
    let mut udp_sock = udp::Socket::new(udp_rx, udp_tx);
    udp_sock
        .bind(1234)
        .map_err(|e| println!("[net-server] udp bind failed: {:?}", e))
        .ok();

    // Сокет обязан быть в SocketSet, иначе poll() его не опрашивает и никакого
    // исходящего трафика (а значит, и ARP-резолвинга) не будет. handle нужен,
    // чтобы потом достать сокет обратно для send().
    // ICMP-сокет для ping. ident здесь один на весь процесс, как у настоящего
    // ping; seq_no растёт, по нему же сверяем ответ и считаем RTT.
    let icmp_rx = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0u8; 1024]);
    let icmp_tx = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0u8; 256]);
    let mut icmp_sock = icmp::Socket::new(icmp_rx, icmp_tx);
    // smoltcp сам отбрасывает Echo с чужим ident, если сокет привязан к Ident.
    icmp_sock
        .bind(icmp::Endpoint::Ident(PING_IDENT))
        .map_err(|e| println!("[net-server] icmp bind failed: {:?}", e))
        .ok();

    // Сокеты обязаны быть в SocketSet, иначе poll() их не опрашивает.
    let mut sockets = SocketSet::new(vec![]);
    let udp_handle = sockets.add(udp_sock);
    let icmp_handle = sockets.add(icmp_sock);

    // 6. Event loop
    println!("[net-server] entering main event loop");
    let mut idle_polls: u64 = 0;
    let mut iface_routes_ready = false;
    let mut ping = PingState::new(PING_IDENT, gateway);
    // Шлюз ещё не в neighbor cache: пока не разрезолвен MAC, ICMP-пакеты
    // smoltcp буферизует и они не уходят. Поэтому первый echo ждёт ARP.
    let mut arp_ready = false;
    let mut last_diag_ms: u64 = 0;
    let mut polls_this_sec: u64 = 0;
    println!("[net-server] ping: {} packets to {}, interval={}ms timeout={}ms",
        PING_COUNT, gateway, PING_INTERVAL_MS, PING_TIMEOUT_MS);
    loop {
        // Настоящее монотонное время вместо счётчика итераций: smoltcp
        // сравнивает timestamps с таймаутами сокетов, и растущий счётчик
        // вёл себя как часы с произвольной скоростью.
        let now_ms = sys_time();
        let now = Instant::from_millis(now_ms as i64);
        iface.poll(now, &mut device, &mut sockets);

        // 5d: ping. Приём EchoReply: сверяем ident/seq и печатаем RTT.
        // smoltcp отдаёт полезную нагрузку и адрес источника; ident он уже
        // проверил сам при bind(Endpoint::Ident(..)). seq лежит в payload-echo.
        // Буфер приёма заметно больше payload: recv_slice кладёт туда весь
        // ICMP-пакет, и при размере ровно 32 приходил Truncated.
        let mut rx_buf = [0u8; 128];
        loop {
            let recv = sockets.get_mut::<icmp::Socket>(icmp_handle).recv_slice(&mut rx_buf);
            match recv {
                Ok((n, src)) => {
                    let now_ms = sys_time();
                    let src_v4 = match src { IpAddress::Ipv4(v) => v, _ => continue };
                    // Наш payload начинается с "pindos-icmp-ping" — по нему
                    // узнаём, что это ответ на наш запрос.
                    // recv_slice отдаёт весь ICMP-пакет: 8 байт заголовка
                    // (type, code, cksum, ident, seq) и дальше payload.
                    if n < ICMP_ECHO_HDR_LEN { continue; }
                    let ident = u16::from_be_bytes([rx_buf[4], rx_buf[5]]);
                    let seq  = u16::from_be_bytes([rx_buf[6], rx_buf[7]]);
                    let payload = &rx_buf[ICMP_ECHO_HDR_LEN..n];
                    let ours = payload.len() >= 8 && &payload[..8] == b"pindos-";
                    match ping.on_reply(now_ms, ident, seq) {
                        Some(rtt) => println!("[ping] reply from {} seq={} rtt={}ms", src_v4, seq, rtt),
                        None => println!("[ping] reply from {} ident={:#06x} seq={} — not ours (ours={})",
                            src_v4, ident, seq, ours),
                    }
                }
                Err(_) => break,
            }
        }
        if ping.done() {
            println!("[ping] finished: {} sent, {} replied, {} timed out",
                ping.sent, ping.replies, ping.timeouts);
            if SELFTEST_PING {
                return 0;
            }
            // Обычный режим: тест отработал, но процесс остаётся сервисом.
            // Новых запросов не будет, poll loop продолжает крутиться.
            println!("[net-server] ping test done, staying in service loop");
            ping.disable();
        }

        // Отправляем очередной echo, если пора и ещё не исчерпали счётчик.
        if !arp_ready && device.arp_reply_rx > 0 {
            arp_ready = true;
        }

        // Отправку НЕ блокируем ожиданием ARP: smoltcp сам инициирует
        // ARP-резолвинг шлюза при первой попытке отправки и доотправляет
        // пакет после ответа. Ожидание arp_ready здесь давало deadlock —
        // инициатора ARP без работающего ping не существовало.
        if ping.should_send(now_ms) {
            let seq = ping.take_next_seq();
            let repr = Icmpv4Repr::EchoRequest {
                ident: PING_IDENT,
                seq_no: seq,
                data: &PING_PAYLOAD,
            };
            // send_with передаёт сам Repr: smoltcp сам собирает ICMP-пакет и
            // считает контрольную сумму.
            // send_with передаёт Repr целиком: smoltcp сам сериализует пакет
            // и считает контрольную сумму.
            let pkt_len = repr.buffer_len();
            let caps = smoltcp::phy::ChecksumCapabilities::ignored();
            let r = sockets.get_mut::<icmp::Socket>(icmp_handle).send_with(
                pkt_len,
                IpAddress::Ipv4(gateway),
                |raw| {
                    // Icmpv4Packet::new_checked проверяет длину и заголовок,
                    // repr.emit собирает тело и контрольную сумму.
                    match Icmpv4Packet::new_checked(raw) {
                        Ok(mut pkt) => {
                            repr.emit(&mut pkt, &caps);
                            repr.buffer_len()
                        }
                        Err(_) => 0,
                    }
                },
            );
            match r {
                Ok(_) => { ping.on_sent(seq, now_ms); }
                Err(e) => {
                    if idle_polls % 2000 == 0 {
                        println!("[ping] send error: {:?}", e);
                    }
                }
            }
        }

        // Таймауты: 2 секунды на ответ.
        for seq in ping.take_timeouts(now_ms) {
            println!("[ping] seq={} timeout", seq);
        }
        if device.arp_reply_rx > 0 && !iface_routes_ready {
            iface_routes_ready = true;
            println!("[net-server] ARP resolved: gateway {} answered", gateway);
        }

        // poll() ничего не отдал — не сжигаем квант, отдаём CPU соседям.
        // Иначе процесс, которому нечего обрабатывать, занимает квант
        // целиком и dealduck ждёт следующего тика.
        idle_polls += 1;

        // Пункт 2: раз в секунду снимаем состояние ICMP и интерфейса.
        let diag_ms = sys_time();
        if DIAG && diag_ms.wrapping_sub(last_diag_ms) >= 1000 {
            last_diag_ms = diag_ms;
            let (tx_used, tx_cap) = {
                let sk = sockets.get_mut::<icmp::Socket>(icmp_handle);
                (sk.payload_send_capacity(), sk.packet_send_capacity())
            };
            let delay = iface.poll_delay(now, &sockets);
            // Статус устройства: 0x07 = DRIVER_OK, 0x47 = NEEDS_RESET.
            let st = unsafe { virtio::read_status(pci_dev.bar0) };
            println!("[diag] polls/s={} frames_rx={} arp_replies={} icmp_tx_left={} tx_cap={} poll_delay={:?} in_flight={} status={:#04x}",
                polls_this_sec, device.frames_rx, device.arp_reply_rx, tx_used, tx_cap, delay, ping.n_in_flight, st);
            polls_this_sec = 0;
        } else {
            polls_this_sec += 1;
        }

        sys_yield();
    }
}

/// Монотонное время в мс с загрузки (syscall 4).
fn sys_time() -> u64 {
    let ms: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 4u64,
            lateout("rax") ms,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    if ms < 0 { 0 } else { ms as u64 }
}

/// Отдать остаток кванта (syscall 0).
fn sys_yield() {
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 0u64,
            lateout("rax") _,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
}

fn dbg_outb(val: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") 0x3f8u16, in("al") val, options(nostack)); }
}
fn dbg_str(s: &str) {
    for &b in s.as_bytes() {
        dbg_outb(b);
    }
}

#[no_mangle]
pub extern "C" fn rust_eh_personality() {}
#[no_mangle]
pub unsafe extern "C" fn _Unwind_Resume() { loop {} }
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    let mut i = 0;
    while *s.add(i) != 0 { i += 1; }
    i
}
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if src < dest as *const u8 {
        for i in (0..n).rev() { core::ptr::write_volatile(dest.add(i), core::ptr::read_volatile(src.add(i))); }
    } else {
        for i in 0..n { core::ptr::write_volatile(dest.add(i), core::ptr::read_volatile(src.add(i))); }
    }
    dest
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe {
        // write "PANIC\n" to COM1 via outb
        let s = b"PANIC\n";
        for &b in s {
            core::arch::asm!("out dx, al", in("dx") 0x3f8u16, in("al") b, options(nostack));
        }
    }
    // 1 = код паники; dealduck различает его по waitpid и не перезапустит сервис.
    sys_exit(1)
}

// ── 5d: ICMP echo (ping) до шлюза QEMU ───────────────────────────────────────

/// ident нашего ping. Настоящий ping берёт его из PID.
/// Печать раз в секунду: счётчики poll, состояние приёма, ICMP-буфер, статус
/// устройства. Под флагом только печать — ни портов, ни памяти.
const DIAG: bool = false;

/// Пропустить ping-тест при старте и выйти с кодом 0 после него.
///
/// В обычной загрузке выключено: net-server должен остаться постоянным
/// сервисом в poll loop, а тест — это самопроверка сборки. С включённым флагом
/// процесс отрабатывает тест один раз и завершается с 0, что удобно для
/// автоматической проверки в CI.
const SELFTEST_PING: bool = false;

const PING_IDENT: u16 = 0x1234;
/// Сколько echo-запросов отправляем.
const PING_COUNT: u32 = 4;
/// Интервал между запросами, мс.
const PING_INTERVAL_MS: u64 = 1000;
/// Сколько ждём ответа на конкретный seq, мс.
const PING_TIMEOUT_MS: u64 = 2000;

/// Заголовок ICMP echo: type(1) code(1) cksum(2) ident(2) seq(2).
const ICMP_ECHO_HDR_LEN: usize = 8;

const PING_PAYLOAD: [u8; 32] = [
    b'p', b'i', b'n', b'd', b'o', b's', b'-', b'i', b'c', b'm', b'p', b'-', b'p', b'i', b'n', b'g',
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

/// Состояние ping: счётчики, время отправки по seq, ожидающие ответа.
struct PingState {
    ident:       u16,
    target:      Ipv4Address,
    next_seq:    u16,
    sent:        u32,
    replies:     u32,
    timeouts:    u32,
    /// seq -> время отправки (мс). Ограниченный массив: PING_COUNT записей.
    in_flight:   [(u16, u64); 8],
    n_in_flight: usize,
    last_send:   u64,
    /// Тест отработал и больше не должен слать запросы.
    finished:    bool,
}

impl PingState {
    fn new(ident: u16, target: Ipv4Address) -> Self {
        Self {
            ident,
            target,
            next_seq: 0,
            sent: 0,
            replies: 0,
            timeouts: 0,
            in_flight: [(0, 0); 8],
            n_in_flight: 0,
            last_send: 0,
            finished: false,
        }
    }

    /// Пора ли слать следующий запрос: интервал вышел и лимит не исчерпан.
    fn should_send(&self, now_ms: u64) -> bool {
        if self.finished { return false; }
        if self.sent >= PING_COUNT { return false; }
        if self.sent == 0 { return true; }
        now_ms.wrapping_sub(self.last_send) >= PING_INTERVAL_MS
    }

    /// Выдаёт следующий seq и сразу увеличивает счётчик.
    fn take_next_seq(&mut self) -> u16 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        seq
    }

    fn on_sent(&mut self, seq: u16, now_ms: u64) {
        self.sent += 1;
        self.last_send = now_ms;
        if self.n_in_flight < self.in_flight.len() {
            self.in_flight[self.n_in_flight] = (seq, now_ms);
            self.n_in_flight += 1;
        }
    }

    /// Ответ: сверяем ident и seq с одним из ожидающих, возвращаем RTT.
    fn on_reply(&mut self, now_ms: u64, ident: u16, seq: u16) -> Option<u64> {
        if ident != self.ident { return None; }
        for i in 0..self.n_in_flight {
            if self.in_flight[i].0 == seq {
                let sent_at = self.in_flight[i].1;
                self.in_flight[i] = self.in_flight[self.n_in_flight - 1];
                self.n_in_flight -= 1;
                self.replies += 1;
                return Some(now_ms.wrapping_sub(sent_at));
            }
        }
        None
    }

    /// Возвращает seq, по которым ответ не пришёл за PING_TIMEOUT_MS.
    fn take_timeouts(&mut self, now_ms: u64) -> alloc::vec::Vec<u16> {
        let mut out = alloc::vec::Vec::new();
        let mut i = 0;
        while i < self.n_in_flight {
            let (seq, sent_at) = self.in_flight[i];
            if now_ms.wrapping_sub(sent_at) >= PING_TIMEOUT_MS {
                self.timeouts += 1;
                out.push(seq);
                self.in_flight[i] = self.in_flight[self.n_in_flight - 1];
                self.n_in_flight -= 1;
            } else {
                i += 1;
            }
        }
        out
    }

    fn done(&self) -> bool {
        !self.finished && self.replies + self.timeouts >= PING_COUNT
    }

    /// Завершить тест: новых запросов больше не будет.
    fn disable(&mut self) {
        self.finished = true;
    }
}
