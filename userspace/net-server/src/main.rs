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
    socket::udp,
    time::Instant,
    wire::{EthernetAddress, IpCidr, Ipv4Address},
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
    let mut sockets = SocketSet::new(vec![]);
    let udp_handle = sockets.add(udp_sock);

    // 6. Event loop
    // Тест RX: 600 broadcast-кадров, чтобы убедиться, что публикация буферов
    // не останавливается после qsize кадров. Включается переменной окружения
    // нет, поэтому гоняем всегда и сразу выходим.
    const STRESS_FRAMES: u64 = 600;

    println!("[net-server] entering main event loop");
    let mut idle_polls: u64 = 0;
    let mut iface_routes_ready = false;
    loop {
        // Настоящее монотонное время вместо счётчика итераций: smoltcp
        // сравнивает timestamps с таймаутами сокетов, и растущий счётчик
        // вёл себя как часы с произвольной скоростью.
        let now = Instant::from_millis(sys_time() as i64);
        iface.poll(now, &mut device, &mut sockets);

        // smoltcp не начинает ARP-резолвинг, пока некуда отправлять пакет.
        // Периодически шлём UDP на шлюз: это заставляет интерфейс искать его
        // MAC через ARP (и, transitively, даёт трафик для 5d).
        // Стресс: шлём broadcast-кадры, пока used.idx не превысит STRESS_FRAMES.
        if device.frames_rx < STRESS_FRAMES && device.frames_tx < STRESS_FRAMES * 4 {
            if idle_polls % 3 == 0 {
                let _ = sockets.get_mut::<udp::Socket>(udp_handle).send(64, (gateway, 9));
            }
        }
        if device.frames_rx >= STRESS_FRAMES {
            println!("[net-server] RX stress OK: frames_rx={} frames_tx={}", device.frames_rx, device.frames_tx);
            return 0;
        }

        // Диагностика RX-кольца раз в 20000 итераций.
        if idle_polls % 20000 == 0 && idle_polls > 0 {
            unsafe { device.rx_queue.dump_rx_state("poll") };
        }

        if idle_polls % 500 == 0 {
            match sockets.get_mut::<udp::Socket>(udp_handle).send(1, (gateway, 9)) {
                Ok(buf) => buf[0] = 0xAA,
                Err(e) => {
                    if idle_polls % 2000 == 0 {
                        println!("[net-server] gateway probe error: {:?}, frames tx={} rx={}", e, device.frames_tx, device.frames_rx);
                    }
                }
            }
        }
        if device.arp_reply_rx > 0 && !iface_routes_ready {
            iface_routes_ready = true;
            println!("[net-server] ARP resolved: gateway {} answered", gateway);
        }

        // poll() ничего не отдал — не сжигаем квант, отдаём CPU соседям.
        // Иначе процесс, которому нечего обрабатывать, занимает квант
        // целиком и dealduck ждёт следующего тика.
        idle_polls += 1;
        sys_yield();

        if idle_polls % 2000 == 0 {
            println!("[net-server] idle polls: {}", idle_polls);
        }
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
