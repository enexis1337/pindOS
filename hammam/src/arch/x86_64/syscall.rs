use crate::kprintln;
use crate::mm::userptr::validate_user_slice;
use crate::sched::task::AddressSpace;
use crate::process::{PROCESS_TABLE, next_pid, Process};
use crate::vfs::VFS;
use alloc::vec;

/// Kernel stack RSP used by syscall_entry.
pub static mut SC_KERNEL_RSP: u64 = 0;

/// Debug switch dump enabled (set from boot).
pub static mut SC_DEBUG: bool = false;

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
        // SYSCALL не меняет стек: RSP всё ещё указывает на userspace.
        // Сохраняем user RSP на НАШЕМ kernel-стеке первым слотом (не в
        // глобальной переменной — между switch внутри syscall другие задачи
        // работают на своих kernel-стеках и не тронут наш слот).
        "mov r10, rsp",           // user RSP -> r10
        "mov rsp, [{krsp}]",      // switch to this task's kernel stack

        // 10 pushes = 80 bytes, стек команд 16-выровнен.
        // rsp после всех push = top-80:
        "push r10",           // user RSP            -> [rsp+72]
        "push rcx",           // user RIP            -> [rsp+64]
        "push r11",           // user RFLAGS         -> [rsp+56]
        "push rdi",           // save a0             -> [rsp+48]
        "push rsi",           // save a1             -> [rsp+40]
        "push rdx",           // save a2             -> [rsp+32]
        "push r8",            // save a3             -> [rsp+24]
        "push r9",            // save a4             -> [rsp+16]
        "push rbx",           // padding             -> [rsp+8]
        "push rbp",           // padding             -> [rsp+0]

        // Move syscall args to ABI calling convention for dispatch(nr, a0, a1, a2)
        // nr in RAX -> RDI (1st arg)
        // a0 in [rsp+48] -> RSI (2nd arg)
        // a1 in [rsp+40] -> RDX (3rd arg)
        // a2 in [rsp+32] -> RCX (4th arg)
        "mov rdi, rax",       // nr -> RDI (1st arg)
        "mov rsi, [rsp + 48]", // a0 -> RSI (2nd arg)
        "mov rdx, [rsp + 40]", // a1 -> RDX (3rd arg)
        "mov rcx, [rsp + 32]", // a2 -> RCX (4th arg)

        "call {dispatch}",

        // RAX = return value
        // Restore registers (reverse order of push)
        "pop rbp",            // discard padding
        "pop rbx",            // discard padding
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop r11",
        "pop rcx",
        "pop rsp",            // restore user RSP from [top-8] slot

        // Mask R11 (RFLAGS) before sysretq - clear dangerous bits, keep IOPL(12-13)
        "and r11, 0x3FFF",    // keep bits 0..13 (IF=9, IOPL=12-13, DF=10)
        "or r11, 0x200",      // ensure IF=1 (interrupts enabled in userspace)
        "sysretq",

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
        4 => sys_time(),
        5 => sys_dma_alloc(a0, a1),
        60 => sys_exit(a0 as i32),
        _ => -38,
    }
}

fn sys_yield() -> i64 {
    if crate::sched::SCHED_DEBUG { kprintln!("[syscall] yield called"); }
    crate::sched::yield_now();
    0
}

/// Записывает пару u64 в пользовательскую память, предварительно проверив,
/// что адрес принадлежит адресному пространству процесса.
fn write_u64_pair(
    aspace: &crate::sched::task::AddressSpace,
    out: u64,
    a: u64,
    b: u64,
) -> Result<(), ()> {
    // Страницы должны быть отображены и доступны на запись.
    for page in [out, out + 8] {
        match aspace.translate_flags(page) {
            Some(f) if f.contains(crate::mm::PageFlags::WRITABLE) => {}
            _ => return Err(()),
        }
    }
    // SAFETY: адреса проверены выше как отображённые и записываемые.
    unsafe {
        *(out as *mut u64) = a;
        *((out + 8) as *mut u64) = b;
    }
    Ok(())
}

/// dma_alloc(pages, out) — выделяет физически непрерывный блок памяти и
/// мапит его в адресное пространство вызывающего процесса.
///
/// Нужен для DMA: virtio дескрипторы и QUEUE_PFN требуют ФИЗИЧЕСКИХ адресов,
/// а userspace не identity mapped (ELF грузится на USER_BASE, страницы
/// раскладываются по произвольным buddy-фреймам), поэтому передавать туда
/// userspace-виртуальные адреса бессмысленно — устройство прочитает мусор.
///
/// Возвращает 0 при успехе и заполняет out[0] = virt, out[1] = phys.
/// pages округляется вверх до степени двойки (порядка buddy).
///
/// TODO(capabilities): доступ к DMA-памяти должен выдаваться по capability
/// `DmaMemory` с проверкой лимита. Сейчас любой процесс может запросить любой
/// объём, проверки нет.
fn sys_dma_alloc(pages: u64, out: u64) -> i64 {
    const DMA_BASE: u64 = 0x0400_0000; // 64 МиB, выше userspace (0x08000000 стек)
    const DMA_LIMIT: u64 = 0x0800_0000;
    const PAGE: u64 = 0x1000;

    if pages == 0 {
        return -22;
    }

    // Порядок buddy по числу страниц.
    let mut order = 0u8;
    while (1u64 << order) < pages && order < crate::mm::physical::MAX_ORDER as u8 - 1 {
        order += 1;
    }
    let order_pages = 1u64 << order;

    if crate::sched::SCHED_DEBUG { kprintln!("[dma] request {} pages", pages); }
    // Виртуальное окно выдаётся подряд с bumping-счётчика: раньше virt всегда
    // равнялся DMA_BASE, поэтому второй запрос упирался в уже занятую страницу
    // и DMA-памяти хватало ровно на один блок.
    static DMA_NEXT: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(DMA_BASE);
    // Слоты забираем подряд, без гонок за прорезервленное окно.
    let virt = DMA_NEXT.fetch_add(order_pages * PAGE, Ordering::Relaxed);
    if virt < DMA_BASE || virt + order_pages * PAGE > DMA_LIMIT {
        DMA_NEXT.fetch_sub(order_pages * PAGE, Ordering::Relaxed);
        return -12;
    }

    let frame = {
        let mut allocator = crate::mm::PHYSICAL_ALLOCATOR.lock();
        match allocator.allocate(order) {
            Ok(f) => f,
            Err(_) => return -12,
        }
    };

    // Мапим в текущий (активный) адресное пространство процесса.
    {
        let mut allocator = crate::mm::PHYSICAL_ALLOCATOR.lock();
        for i in 0..order_pages {
            let target = crate::mm::PhysFrame::new(frame.start_address + i * PAGE);
            // SAFETY: virt свободен (DMA_BASE вне userspace и ядра), фрейм только что выделен.
            let res = unsafe {
                crate::mm::map_page(
                    virt + i * PAGE,
                    target,
                    // USER_ACCESSIBLE обязателен: процесс работает в ring 3 и
                    // пишет в кольца очереди сам. Без этого бита первая же
                    // запись в desc вызывает #PF и процесс зависает.
                    crate::mm::PageFlags::PRESENT
                        | crate::mm::PageFlags::WRITABLE
                        | crate::mm::PageFlags::USER_ACCESSIBLE,
                    &mut allocator,
                )
            };
            if res.is_err() {
                if crate::sched::SCHED_DEBUG {
                    kprintln!("[dma] map_page failed at {:#x}", virt + i * PAGE);
                }
                return -12;
            }
        }
    }

    // Копируем пару (virt, phys) в пользовательский буфер.
    let user = {
        let current = match crate::sched::get_current_task() {
            Some(t) => t,
            None => return -1,
        };
        let aspace = current.address_space.lock();
        match crate::arch::x86_64::syscall::write_u64_pair(&aspace, out, virt, frame.start_address) {
            Ok(()) => (),
            Err(_) => return -14,
        }
    };
    let _ = user;

    if crate::sched::SCHED_DEBUG {
        kprintln!("[dma] alloc {} pages (order {}): virt={:#x} phys={:#x}",
            pages, order, virt, frame.start_address);
    }
    0
}

/// time() — монотонное время в миллисекундах с загрузки.
///
/// Счётчик APIC-таймера калибруется на 1 мс на тик (см. `calibrate_apic_timer`),
/// поэтому миллисекунды равны числу тиков. Значение монотонное: при переключении
/// задач CR3 меняется, но счётчик живёт в ядре и общий для всех.
fn sys_time() -> i64 {
    crate::arch::x86_64::apic::uptime_millis() as i64
}

/// exit(code) — завершить процесс
fn sys_exit(code: i32) -> i64 {
    // Умирающий процесс определяем по текущей ЗАДАЧЕ, а не по CURRENT_PROCESS:
    // CURRENT_PROCESS заполняется один раз при загрузке ядра и навсегда указывает
    // на PID 1, поэтому раньше exit() любого сервиса помечал зомбием dealduck, а
    // его собственный pid оставался живым и waitpid никогда не срабатывал.
    let task_pid: Option<u32> = crate::sched::get_current_task().map(|t| t.id.0 as u32);

    kprintln!("[syscall] exit({}) pid={:?}", code, task_pid);

    if let Some(pid) = task_pid {
        if let Some(p) = PROCESS_TABLE.lock().get(&pid) {
            p.exit_code.store(code, Ordering::Release);
            p.is_zombie.store(true, Ordering::Release);
        }
    }

    crate::sched::exit_current();
    unreachable!()
}

/// write(fd, buf, count) — вывести данные на serial
/// Найти Process по текущей задаче: pid задачи равен pid процесса.
fn find_process_by_task(task: &crate::sched::task::Task) -> Option<alloc::sync::Arc<crate::process::Process>> {
    let pid = task.id.0 as u32;
    PROCESS_TABLE.lock().get(&pid).cloned()
}

fn sys_write(fd: u64, buf_ptr: u64, len: u64) -> i64 {
    if crate::sched::SCHED_DEBUG {
        kprintln!("[syscall] write: fd={} buf={:#x} len={}", fd, buf_ptr, len);
    }
    // Право записи берём из fd-таблицы процесса, а не из жёсткого fd == 1.
    let current = crate::sched::get_current_task().expect("no current task");

    let proc = match find_process_by_task(current) {
        Some(p) => p,
        None => return -9,
    };
    {
        let fds = proc.fd_table.lock();
        match fds.get(fd as u32) {
            None => return -9,          // EBADF
            Some(e) if !e.writable => return -9,
            _ => {}
        }
    }

    let aspace = current.address_space.lock();

    let slice = match validate_user_slice(&aspace, buf_ptr, len) {
        Ok(s) => s,
        Err(_) => return -14,
    };

    if crate::sched::SCHED_DEBUG { kprintln!("[syscall] write: validated, len={}", slice.len()); }
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
/// База кодирования кода возврата в sys_waitpid: 0 означает «процесс жив»,
/// поэтому код выхода возвращается как EXIT_CODE_BASE + code.
pub const EXIT_CODE_BASE: i64 = 256;

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
                // Код возврата 0 неотличим от «процесс жив» (оба дают 0),
                // поэтому кодируем выход как EXIT_CODE_BASE + code.
                // Пользователь обязан вычитать базу.
                EXIT_CODE_BASE + code as i64
            } else if wnohang {
                0
            } else {
                -11
            }
        }
    }
}

/// Прыжок в userspace через SYSRET.
///
/// ВНИМАНИЕ (IOPL): в RFLAGS ниже намеренно стоит 0x3202 — это IOPL=3
/// (биты 12-13). Именно IOPL=3 позволяет Ring 3 выполнять `in`/`out` на
/// портах 0xCF8/0xCFC, а без них userspace не может ходить по PCI-шине.
/// Проверено в QEMU: с 0x0202 (IOPL=0) net-server зависает сразу после
/// "starting virtio-net driver..." — первый же портовый доступ ловит #GP.
///
/// TODO(security): IOPL=3 для ВСЕХ процессов оставлять нельзя. Правильное
/// решение по нашей архитектуре — IOPL=0 в userspace плюс I/O permission
/// bitmap в TSS, выдаваемая конкретному процессу по capability `IoPort`
/// (см. план). Bitmap сейчас не инициализируется, поэтому переход на IOPL=0
/// требует сначала реализовать его, иначе PCI-доступ полностью отвалится.
pub unsafe fn jump_to_userspace(entry: u64, stack: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov rcx, {entry}",
            "mov r11, {rflags}",
            "mov rsp, {stack}",
            "xor rbp, rbp",
            "sysretq",
            entry = in(reg) entry,
            rflags = in(reg) 0x3202u64,
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