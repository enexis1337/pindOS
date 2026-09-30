#![no_std]
#![no_main]

mod allocator;
mod println;
mod unit;
mod manager;

use manager::ServiceManager;

/// Монотонное время в миллисекундах с загрузки (syscall 4).
pub fn sys_time() -> u64 {
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

/// Косметическая задержка на N миллисекунд через yield в цикле.
fn sleep_ms(ms: u64) {
    let start = sys_time();
    while sys_time().wrapping_sub(start) < ms {
        unsafe {
            core::arch::asm!("syscall", in("rax") 0u64);
        }
    }
}

/// Проверка sys_time: два замера с паузой, значение должно вырасти.
fn check_time() {
    let t0 = sys_time();
    println!("[dealduck] time: t0={} ms", t0);
    sleep_ms(250);
    let t1 = sys_time();
    println!("[dealduck] time: t1={} ms (delta={} ms)", t1, t1.wrapping_sub(t0));
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Прямой вывод без heap — убедиться что мы вообще сюда попали
    unsafe {
        let msg = b"[dealduck] _start entered\n";
        core::arch::asm!(
            "syscall",
            in("rax") 1u64,
            in("rdi") 1u64,
            in("rsi") msg.as_ptr() as u64,
            in("rdx") msg.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }

    // Test second direct syscall - no heap
    unsafe {
        let msg = b"[dealduck] test2 - no heap\n";
        core::arch::asm!(
            "syscall",
            in("rax") 1u64,
            in("rdi") 1u64,
            in("rsi") msg.as_ptr() as u64,
            in("rdx") msg.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }

    // Debug after second syscall
    unsafe {
        let msg = b"[dealduck] after syscalls, before println\n";
        core::arch::asm!(
            "syscall",
            in("rax") 1u64,
            in("rdi") 1u64,
            in("rsi") msg.as_ptr() as u64,
            in("rdx") msg.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }

    // Теперь первый println! через heap
    println!("[dealduck] PINDOS service manager starting...");
    println!("[dealduck] PID 1");

    let mut manager = ServiceManager::new();

    manager.register("net-server", "/usr/bin/net-server");

    println!("[dealduck] starting services...");
    manager.start_all();

    check_time();

    println!("[dealduck] all services started, entering monitor loop");
    manager.run()
}

extern crate alloc;
use allocator::HEAP;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nostack)) };
    }
}