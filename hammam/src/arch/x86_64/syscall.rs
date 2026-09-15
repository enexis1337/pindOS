use crate::kprintln;
use crate::mm::userptr::validate_user_slice;
use crate::sched::task::AddressSpace;
use crate::process::{PROCESS_TABLE, CURRENT_PROCESS, next_pid, Process};
use crate::vfs::VFS;
use alloc::vec;

/// Saved user RSP during syscall entry (SYSCALL does NOT switch stacks).
pub static mut SC_RSP_SAVE: u64 = 0;
/// Kernel stack RSP used by syscall_entry.
pub static mut SC_KERNEL_RSP: u64 = 0;

/// Ошибки syscall операций
#[derive(Debug, Clone, Copy)]
pub enum SyscallError {
    BadAddress,
    BadFileDescriptor,
    InvalidArgument,
    NotImplemented,
}

impl From<crate::mm::UserPtrError> for SyscallError {
    fn from(err: crate::mm::UserPtrError) -> Self {
        match err {
            crate::mm::UserPtrError::BadAddress => SyscallError::BadAddress,
            crate::mm::UserPtrError::NotMapped => SyscallError::BadAddress,
            crate::mm::UserPtrError::NotWritable => SyscallError::BadAddress,
            crate::mm::UserPtrError::OverflowDetected => SyscallError::InvalidArgument,
        }
    }
}

impl From<SyscallError> for i64 {
    fn from(err: SyscallError) -> i64 {
        match err {
            SyscallError::BadAddress => -14,
            SyscallError::BadFileDescriptor => -9,
            SyscallError::InvalidArgument => -22,
            SyscallError::NotImplemented => -38,
        }
    }
}

// MSR адреса для SYSCALL/SYSRET
const MSR_EFER: u32 = 0xC0000080;
const MSR_STAR: u32 = 0xC0000081;
const MSR_LSTAR: u32 = 0xC0000082;
const MSR_SFMASK: u32 = 0xC0000084;

// Биты EFER
const EFER_SCE: u64 = 1 << 0;

// Маска для SFMASK — маскировать IF (interrupt flag, бит 9)
const SFMASK_IF: u64 = 1 << 9;

use crate::arch::gdt;

/// Установить kernel stack для syscall_entry
pub fn set_kernel_stack(rsp: u64) {
    unsafe { SC_KERNEL_RSP = rsp; }
}

/// Инициализация SYSCALL/SYSRET механизма
pub fn init() {
    unsafe {
        let mut efer = rdmsr(MSR_EFER);
        efer |= EFER_SCE;
        wrmsr(MSR_EFER, efer);

        let kernel_code = gdt::KERNEL_CODE as u64;         // 0x08
        let user_base = gdt::SYSRET_USER_BASE as u64;      // 0x18 (STAR[63:48])
        // STAR layout:
        //   [47:32] = kernel code -> SYSCALL: CS=0x08, SS=0x08+8=0x10 (kernel data)
        //   [63:48] = user base   -> SYSRET:  SS=base+8=0x20, CS=base+16=0x28
        let star = (user_base << 48) | (kernel_code << 32);
        wrmsr(MSR_STAR, star);

        let syscall_entry_ptr = syscall_entry as *const () as u64;
        wrmsr(MSR_LSTAR, syscall_entry_ptr);

        wrmsr(MSR_SFMASK, SFMASK_IF);
    }
}

/// Чтение из Model Specific Register
#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let high: u32;
    let low: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nostack, preserves_flags)
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// Запись в Model Specific Register
#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    let high = (value >> 32) as u32;
    let low = value as u32;
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags)
        );
    }
}

/// Точка входа из userspace — голый asm без Rust пролога.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // Вход: RAX=nr, RDI=a0, RSI=a1, RDX=a2, RCX=RIP, R11=RFLAGS
        // Сохранить user RSP
        "mov [{saved}], rsp",
        "mov rsp, [{krsp}]",

        // Stack must be 16-byte aligned before CALL
        "push rcx",           // save user RIP           -> [rsp+56]
        "push r11",           // save user RFLAGS        -> [rsp+48]
        "push rdi",           // save a0                 -> [rsp+40]
        "push rsi",           // save a1                 -> [rsp+32]
        "push rdx",           // save a2                 -> [rsp+24]
        "push r8",            // save arg3               -> [rsp+16]
        "push r9",            // save arg4               -> [rsp+8]
        "push r10",           // padding for alignment   -> [rsp+0]
        // Stack now 16-byte aligned (8 pushes = 64 bytes)

        // Move syscall args to ABI calling convention for dispatch(nr, a0, a1, a2)
        // nr in RAX -> RDI (1st arg)
        // a0 in [rsp+40] -> RSI (2nd arg)
        // a1 in [rsp+32] -> RDX (3rd arg)
        // a2 in [rsp+24] -> RCX (4th arg)
        "mov rdi, rax",       // nr -> RDI (1st arg)
        "mov rsi, [rsp + 40]", // a0 -> RSI (2nd arg)
        "mov rdx, [rsp + 32]", // a1 -> RDX (3rd arg)
        "mov rcx, [rsp + 24]", // a2 -> RCX (4th arg)

        "call {dispatch}",

        // RAX = return value
        // Restore registers (reverse order of push)
        "pop r10",            // discard padding
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop r11",
        "pop rcx",

        // Restore user RSP
        "mov rsp, [{saved}]",

        // Mask R11 (RFLAGS) before sysretq - clear NT(14), VM(17), and other dangerous bits
        "and r11, 0x3FFF",    // clear bits 14+ (NT=14, VM=17, RF=16, etc.)
        "or r11, 0x200",      // ensure IF=1 (interrupts enabled in userspace)
        "sysretq",

        saved    = sym SC_RSP_SAVE,
        krsp     = sym SC_KERNEL_RSP,
        dispatch = sym syscall_dispatch,
    );
}

/// Rust диспетчер syscall
#[no_mangle]
pub extern "C" fn syscall_dispatch(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    match nr {
        0 => sys_yield(),
        1 => sys_write(a0, a1, a2),
        2 => sys_exec(a0, a1),
        3 => sys_waitpid(a0, a1),
        60 => sys_exit(a0 as i32),
        _ => -38,
    }
}

fn sys_yield() -> i64 {
    kprintln!("[syscall] yield called");
    crate::sched::yield_now();
    0
}

/// exit(code) — завершить процесс
fn sys_exit(code: i32) -> i64 {
    kprintln!("[syscall] exit({})", code);
    if let Some(proc) = CURRENT_PROCESS.lock().as_ref() {
        let pid = proc.pid;
        if let Some(p) = PROCESS_TABLE.lock().get(&pid) {
            p.exit_code.store(code, Ordering::Release);
            p.is_zombie.store(true, Ordering::Release);
        }
    }
    crate::sched::exit_current();
    unreachable!()
}

/// write(fd, buf, count) — вывести данные на serial
fn sys_write(fd: u64, buf_ptr: u64, len: u64) -> i64 {
    kprintln!("[syscall] write: fd={} buf={:#x} len={}", fd, buf_ptr, len);
    if fd != 1 {
        return -9;
    }

    let current = crate::sched::get_current_task().expect("no current task");
    let aspace = current.address_space.lock();

    let slice = match validate_user_slice(&aspace, buf_ptr, len) {
        Ok(s) => s,
        Err(_) => return -14,
    };

    kprintln!("[syscall] write: validated, len={}", slice.len());
    let prefix = b"[USERSPACE] ";
    unsafe {
        for &b in prefix {
            crate::drivers::serial::SERIAL.get().write_byte(b);
        }
    }
    for &b in slice {
        unsafe { crate::drivers::serial::SERIAL.get().write_byte(b); }
    }

    len as i64
}

/// exec(path) — запустить новый процесс
fn sys_exec(path_ptr: u64, path_len: u64) -> i64 {
    let current = crate::sched::get_current_task().expect("no current task");
    let aspace = current.address_space.lock();

    let path_bytes = match validate_user_slice(&aspace, path_ptr, path_len) {
        Ok(s) => s,
        Err(_) => return -14,
    };
    let path = match core::str::from_utf8(path_bytes) {
        Ok(s) => s,
        Err(_) => return -22,
    };

    kprintln!("[syscall] exec: {}", path);

    let vnode = match VFS.lock().lookup(path) {
        Ok(v) => v,
        Err(_) => return -2,
    };

    let stat = match vnode.stat() {
        Ok(s) => s,
        Err(_) => return -5,
    };

    let mut elf_data = vec![0u8; stat.size as usize];
    if let Err(_) = vnode.read(0, &mut elf_data) {
        return -5;
    }

    let process = match Process::from_elf(next_pid(), &elf_data) {
        Ok(p) => p,
        Err(_) => return -12,
    };

    let pid = process.pid;
    let process_arc = alloc::sync::Arc::new(process);

    crate::sched::SCHEDULER.lock().add_task(process_arc.main_task.clone());
    PROCESS_TABLE.lock().insert(pid, alloc::sync::Arc::clone(&process_arc));

    kprintln!("[syscall] exec: spawned pid={}", pid);
    pid as i64
}

/// waitpid(pid, flags) — ожидать завершения процесса
fn sys_waitpid(pid: u64, flags: u64) -> i64 {
    let wnohang = flags & 1 != 0;
    let pid = pid as u32;

    let table = PROCESS_TABLE.lock();
    match table.get(&pid) {
        None => -10,
        Some(proc) => {
            if proc.is_zombie() {
                let code = proc.exit_code.load(Ordering::Acquire);
                drop(table);
                PROCESS_TABLE.lock().remove(&pid);
                code as i64
            } else if wnohang {
                0
            } else {
                -11
            }
        }
    }
}

/// Прыжок в userspace через SYSRET.
pub unsafe fn jump_to_userspace(entry: u64, stack: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov rcx, {entry}",
            "mov r11, {rflags}",
            "mov rsp, {stack}",
            "xor rbp, rbp",
            "sysretq",
            entry = in(reg) entry,
            rflags = in(reg) 0x202u64,
            stack = in(reg) stack,
            options(noreturn)
        )
    }
}

/// Trampoline for scheduler to re-enter userspace via SYSRET.
/// Called when scheduler switches to a process that was started via SYSRET.
/// Gets process entry point and user stack from current task.
#[no_mangle]
pub extern "C" fn return_to_userspace_trampoline() -> ! {
    kprintln!("[trampoline] re-entering userspace");
    let (entry, stack) = {
        if let Some(task) = crate::sched::get_current_task() {
            (task.user_entry, task.user_stack)
        } else {
            kprintln!("[trampoline] ERROR: no current task!");
            loop { unsafe { core::arch::asm!("hlt", options(nostack)); } }
        }
    };
    kprintln!("[trampoline] entry={:#x} stack={:#x}", entry, stack);
    unsafe { jump_to_userspace(entry, stack); }
}

use core::sync::atomic::Ordering;
use alloc::sync::Arc;