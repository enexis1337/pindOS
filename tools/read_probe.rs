//! Temporary userspace probe for tools/test_read.py; not installed in normal ISO.
#![no_std]
#![no_main]

unsafe fn call(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let result: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => result,
        in("rdi") a0, in("rsi") a1, in("rdx") a2,
        // syscall_entry использует r10 как scratch для user RSP.
        lateout("rcx") _, lateout("r10") _, lateout("r11") _,
    );
    result
}

fn print(bytes: &[u8]) {
    unsafe { call(1, 1, bytes.as_ptr() as u64, bytes.len() as u64); }
}

fn hex(value: u64) {
    let mut bytes = [0u8; 17];
    for i in 0..16 {
        bytes[i] = b"0123456789abcdef"[((value >> ((15 - i) * 4)) & 15) as usize];
    }
    bytes[16] = b'\n';
    print(&bytes);
}

fn read(fd: u64, ptr: *mut u8, len: u64) -> i64 {
    unsafe { call(6, fd, ptr as u64, len) }
}

#[cfg(not(feature = "broadcast"))]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut buf = [0xa5u8; 16];
    let ptr = buf.as_mut_ptr();
    assert_eq!(read(0, core::ptr::null_mut(), 0), 0);
    for fd in [1, 2, 3, u32::MAX as u64, 1u64 << 32, u64::MAX] {
        assert_eq!(read(fd, ptr, 1), -9);
    }
    assert_eq!(read(0, core::ptr::null_mut(), 1), -14);
    assert_eq!(read(0, (u64::MAX - 1) as *mut u8, 4), -14);
    assert_eq!(read(0, 0x8000_0000_0000usize as *mut u8, 1), -14);
    // ELF .rodata is mapped read-only by the real loader.
    assert_eq!(read(0, b"readonly".as_ptr() as *mut u8, 1), -14);
    print(b"[read-probe] PASS zero length / EBADF / bad and readonly buffers\n");

    print(b"[read-probe] WAIT chunked input\n");
    let expected = b"abcdef";
    let mut offset = 0;
    while offset < expected.len() {
        let limit = core::cmp::min(3, expected.len() - offset);
        let n = read(0, ptr, limit as u64);
        print(b"[read-probe] read count: ");
        hex(n as u64);
        assert!(n > 0 && n <= limit as i64);
        for i in 0..n as usize {
            assert_eq!(buf[i], expected[offset + i]);
            buf[i] = 0xa5;
        }
        assert_eq!(buf[limit], 0xa5);
        offset += n as usize;
    }
    print(b"[read-probe] PASS bounded reads: abcdef\n");

    print(b"[read-probe] WAIT one byte (len=16)\n");
    assert_eq!(read(0, ptr, 16), 1);
    assert_eq!(buf[0], b'Z');
    for i in 1..16 {
        assert_eq!(buf[i], 0xa5);
    }
    print(b"[read-probe] PASS partial read: Z\n");
    let path = b"/usr/bin/wake-probe";
    for _ in 0..2 {
        assert!(unsafe { call(2, path.as_ptr() as u64, path.len() as u64, 0) } > 0);
    }
    print(b"[read-probe] ALL PASS\n");
    unsafe { call(60, 0, 0, 0); }
    loop { core::hint::spin_loop(); }
}

#[cfg(feature = "broadcast")]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut buf = [0u8; 16];
    print(b"[wake-probe] WAIT\n");
    assert_eq!(read(0, buf.as_mut_ptr(), 16), 1);
    assert!(buf[0] == b'A' || buf[0] == b'B');
    let mut result = *b"[wake-probe] GOT ?\n";
    result[17] = buf[0];
    print(&result);
    unsafe { call(60, 0, 0, 0); }
    loop { core::hint::spin_loop(); }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    print(b"[read-probe] FAIL line: ");
    hex(info.location().map(|l| l.line()).unwrap_or(0) as u64);
    unsafe { call(60, 1, 0, 0); }
    loop { core::hint::spin_loop(); }
}
