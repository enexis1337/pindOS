#![no_std]
#![no_main]

mod line;

fn syscall(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let result: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => result,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            // r10 — scratch для user RSP в текущем syscall_entry.
            lateout("rcx") _,
            lateout("r10") _,
            lateout("r11") _,
        );
    }
    result
}

fn write(bytes: &[u8]) {
    syscall(1, 1, bytes.as_ptr() as u64, bytes.len() as u64);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut line = line::Line::new();
    let mut input = [0u8; 64];
    write(b"[echo-test] waiting for input\n");
    loop {
        let count = syscall(6, 0, input.as_mut_ptr() as u64, input.len() as u64);
        if count < 0 {
            syscall(0, 0, 0, 0);
            continue;
        }
        for &byte in &input[..count as usize] {
            if let Some(output) = line.push(byte) {
                write(output);
            }
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    write(b"[echo-test] PANIC\n");
    loop {
        syscall(0, 0, 0, 0);
    }
}
