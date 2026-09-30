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
    dbg_str("[main] init rx_queue\n");
    let rx_queue = unsafe { virtio::Virtqueue::init(pci_dev.bar0, 0) };
    dbg_str("[main] init tx_queue\n");
    let tx_queue = unsafe { virtio::Virtqueue::init(pci_dev.bar0, 1) };
    dbg_str("[main] queues done\n");

    // 3. Создать Device wrapper для smoltcp
    let mut device = device::VirtioNetDevice {
        rx_queue,
        tx_queue,
        rx_buf: [0u8; 1514],
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
    iface.update_ip_addrs(|addr_list| {
        addr_list.push(IpCidr::new(Ipv4Address::new(10, 0, 0, 2).into(), 24)).ok();
    });
    let mut sockets = SocketSet::new(vec![]);

    // 6. Event loop
    println!("[net-server] entering main event loop");
    let mut poll_count: u64 = 0;
    loop {
        let timestamp = Instant::from_millis(poll_count as i64);
        iface.poll(timestamp, &mut device, &mut sockets);
        poll_count += 1;
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
