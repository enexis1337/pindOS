#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::all)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use alloc::boxed::Box;
use alloc::sync::Arc;

pub mod boot;

pub mod boot_info;
pub mod drivers;
pub mod interrupts;
pub mod mm;
pub mod acpi;
pub mod arch;
pub mod sched;
pub mod smp;
pub mod loader;
pub mod cap;
pub mod process;
pub mod userspace_blob;
pub mod vfs;
pub mod io;
pub mod block;
pub mod security;

use boot_info::{MemoryKind, MemoryRegion};
use core::mem::size_of;
use mm::physical::PHYSICAL_ALLOCATOR;

const MAX_MEMORY_REGIONS: usize = 64;

#[repr(C)]
struct Mb2Tag {
    tag_type: u32,
    tag_size: u32,
}

#[repr(C)]
struct Mb2MmapTag {
    tag_type: u32,
    tag_size: u32,
    entry_size: u32,
    entry_version: u32,
}

#[repr(C)]
struct Mb2MmapEntry {
    base_addr: u64,
    length: u64,
    entry_type: u32,
    reserved: u32,
}

fn parse_multiboot2_memory_map(mbi_ptr: u32, regions: &mut [MemoryRegion]) -> usize {
    let boot_info = mbi_ptr as *const Mb2BootInfo;
    let total_size = unsafe { (*boot_info).total_size };
    let end = (mbi_ptr + total_size) as *const u8;
    let mut tag_ptr = (mbi_ptr + 8) as *const u8;
    let mut count = 0;

    while (tag_ptr as u32 + 8) <= (end as u32) {
        let tag = unsafe { &*(tag_ptr as *const Mb2Tag) };
        if tag.tag_type == 0 {
            break;
        }
        if tag.tag_type == 6 {
            let mmap = unsafe { &*(tag_ptr as *const Mb2MmapTag) };
            let entry_size = mmap.entry_size as usize;
            let entries_byte_count = (mmap.tag_size as usize).saturating_sub(size_of::<Mb2MmapTag>());
            let entries_start = unsafe { tag_ptr.add(size_of::<Mb2MmapTag>()) };
            let mut offset: usize = 0;

            while offset + entry_size <= entries_byte_count && count < regions.len() {
                let entry = unsafe { &*((entries_start as usize + offset) as *const Mb2MmapEntry) };
                let kind = match entry.entry_type {
                    1 => MemoryKind::Usable,
                    3 => MemoryKind::AcpiReclaimable,
                    4 => MemoryKind::AcpiNvs,
                    _ => MemoryKind::Reserved,
                };
                regions[count] = MemoryRegion {
                    start: entry.base_addr,
                    end: entry.base_addr + entry.length,
                    kind,
                };
                count += 1;
                offset += entry_size;
            }
        }
        let aligned_size = ((tag.tag_size + 7) & !7) as usize;
        tag_ptr = unsafe { tag_ptr.add(aligned_size) };
    }

    count
}

#[repr(C)]
struct Mb2BootInfo {
    total_size: u32,
    reserved: u32,
}

/// Точка входа ядра из assembly (_hammam_entry).
#[no_mangle]
pub extern "C" fn _start_multiboot2(magic: u32, mbi_ptr: u32) -> ! {
    unsafe {
        drivers::serial::SERIAL.get().init();
        // Приём COM1: включаем IER бит 0 (RDI). Данные забирает опрос LSR
        // из таймерного тика, т.к. маршрутизации через IO-APIC пока нет.
        drivers::serial::SERIAL.get().enable_rx_interrupt();
    }

    kprintln!("Hammam / PINDOS booting...");
    kprintln!("magic = {:#x}", magic);
    kprintln!("mbi   = {:#x}", mbi_ptr);

    arch::x86_64::gdt::init();
    kprintln!("[OK] GDT initialized");

    arch::x86_64::pic::remap();
    arch::x86_64::pic::disable();
    kprintln!("[OK] Legacy PIC remapped and disabled");

    // SAFETY: Called once during boot, no interrupts are enabled yet.
    unsafe { interrupts::init_idt(); }
    kprintln!("[OK] IDT initialized");

    security::enable_smep_smap();
    security::enable_nx();
    security::enable_sse();
    security::init_canary();
    kprintln!("[OK] Security features enabled (SMEP/SMAP/NX/canary)");

    let mut regions = [MemoryRegion {
        start: 0,
        end: 0,
        kind: MemoryKind::Reserved,
    }; MAX_MEMORY_REGIONS];

    let region_count = parse_multiboot2_memory_map(mbi_ptr, &mut regions);
    kprintln!("[OK] Multiboot2 memory map: {} regions", region_count);

    if region_count > 0 {
        unsafe { PHYSICAL_ALLOCATOR.lock().init(&regions[..region_count]); }
        kprintln!("[OK] Buddy allocator initialized");
    }

    unsafe { mm::HEAP_ALLOCATOR.init(); }
    kprintln!("[OK] Heap allocator initialized");

    let v: Vec<u32> = vec![1, 2, 3];
    kprintln!("heap test: {:?}", v);

    // Mount initramfs
    let cpio_data = userspace_blob::build_initramfs();
    // Leak the Vec to get 'static lifetime for initramfs parser
    let cpio_data_leaked: &'static [u8] = Box::leak(cpio_data.into_boxed_slice());
    let initramfs = vfs::initramfs::InitramfsFs::parse(cpio_data_leaked)
        .expect("initramfs parse failed");
    let root_vnode = initramfs.lookup_path("/")
        .expect("initramfs root lookup failed");
    // Leak the Arc so raw pointers in Vnodes stay valid
    let _ = Arc::into_raw(initramfs);
    vfs::VFS.lock().mount("/", root_vnode);
    kprintln!("[OK] Initramfs mounted");

    // Load /init ELF (dealduck = PID 1)
    let init_vnode = vfs::VFS.lock().lookup("/init")
        .expect("initramfs must contain /init");
    let stat = init_vnode.stat().expect("stat failed");
    let mut elf_data = alloc::vec![0u8; stat.size as usize];
    init_vnode.read(0, &mut elf_data).expect("read failed");
    kprintln!("[OK] /init (dealduck) loaded ({} bytes)", elf_data.len());

    // Initialize SYSCALL/SYSRET
    arch::x86_64::syscall::init();
    kprintln!("[OK] SYSCALL/SYSRET initialized");

    // Initialize Local APIC timer for scheduler ticks.
    // Делаем это ДО создания первого процесса: отображение MMIO APIC должно
    // попасть в эталонные boot-таблицы, из которых клонируются скелеты всех
    // процессов (иначе первый тик таймера даст #PF в адресном пространстве PID 1).
    arch::x86_64::apic::init_timer_only();
    kprintln!("[OK] Local APIC timer initialized");

    // Load ELF via Process as PID 1
    let init_process = process::Process::from_elf(1, &elf_data)
        .expect("failed to create init process");

    let init_process_arc = Arc::new(init_process);

    // Контекст задачи для scheduler (trampoline, CR3, user_entry/user_stack)
    // уже проинициализирован в Process::from_elf.

    // Add dealduck to scheduler and set as current
    crate::sched::SCHEDULER.lock().add_task(init_process_arc.main_task.clone());
    process::PROCESS_TABLE.lock().insert(1, Arc::clone(&init_process_arc));
    process::CURRENT_PROCESS.lock().replace(Arc::clone(&init_process_arc));

    kprintln!("[OK] PID 1 (dealduck) created, entry={:#x}", init_process_arc.entry_point);

    kprintln!("[OK] Starting scheduler with dealduck as initial task");
    crate::sched::start_scheduler();
    loop {
        unsafe { core::arch::asm!("hlt", options(nostack)); }
    }
}

/// Обработчик паник ядра Hammam.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("\n!!! KERNEL PANIC !!!");
    if let Some(location) = info.location() {
        kprintln!("Location: {}:{}:{}", location.file(), location.line(), location.column());
    }
    kprintln!("Message: {}", info);
    kprintln!("====================================================");

    loop {
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
