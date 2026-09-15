use core::mem;

// Per-CPU interrupt stack (IST1)
#[repr(align(4096))]
struct InterruptStack([u8; 8192]);

static mut INTERRUPT_STACK: InterruptStack = InterruptStack([0; 8192]);

// Селекторы сегментов с RPL (Requested Privilege Level)
// Порядок дескрипторов обязателен для SYSCALL/SYSRET (Intel SDM 5.8.8):
//   STAR[47:32] = 0x08 (kernel code)  -> SYSCALL: CS=0x08, SS=0x08+8=0x10
//   STAR[63:48] = 0x18 (user pair)    -> SYSRET:  SS=0x18+8=0x20, CS=0x18+16=0x28
pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const SYSRET_USER_BASE: u16 = 0x18;  // STAR[63:48], в GDT — заглушка
pub const USER_DATA: u16 = 0x20 | 3;     // RPL=3
pub const USER_CODE: u16 = 0x28 | 3;     // RPL=3
pub const TSS_SELECTOR: u16 = 0x30;

/// Task State Segment (TSS) для x86-64
/// Используется для хранения rsp0 - kernel stack pointer при переходе из Ring 3
#[repr(C, packed(1))]
pub struct Tss {
    reserved0: u32,
    pub rsp0: u64,      // Kernel stack pointer - загружается при syscall из Ring 3
    pub rsp1: u64,
    pub rsp2: u64,
    reserved1: u64,
    pub ist1: u64,      // Interrupt Stack Table
    pub ist2: u64,
    pub ist3: u64,
    pub ist4: u64,
    pub ist5: u64,
    pub ist6: u64,
    pub ist7: u64,
    reserved2: u64,
    reserved3: u16,
    pub iomap_base: u16,
}

impl Tss {
    pub const fn new() -> Self {
        Tss {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist1: 0,
            ist2: 0,
            ist3: 0,
            ist4: 0,
            ist5: 0,
            ist6: 0,
            ist7: 0,
            reserved2: 0,
            reserved3: 0,
            iomap_base: mem::size_of::<Tss>() as u16,
        }
    }
}

/// GDT дескриптор сегмента (8 байт)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SegmentDescriptor(u64);

impl SegmentDescriptor {
    /// Null дескриптор
    pub const fn null() -> Self {
        SegmentDescriptor(0)
    }

    /// Code сегмент (Ring 0 или Ring 3)
    pub const fn code(dpl: u8) -> Self {
        // Собираем дескриптор напрямую в финальных битах, чтобы DPL не терялся.
        //   bits 55:48 = Flags(G=1,D=0,L=1,AVL=0)=0xA0 | Limit[19:16]=0xF  -> 0xAF
        //   bits 47:40 = Access: P=1, DPL, S=1, Type=1011(code)
        let access = 0x9B | ((dpl & 3) << 5);
        SegmentDescriptor(0x00AF_0000_0000_0000 | ((access as u64) << 40) | 0xFFFF)
    }

    /// Data сегмент (Ring 0 или Ring 3)
    pub const fn data(dpl: u8) -> Self {
        //   bits 55:48 = Flags(G=1,D=0,L=1,AVL=0)=0xA0 | Limit[19:16]=0xF  -> 0xAF
        //   bits 47:40 = Access: P=1, DPL, S=1, Type=0011(data, read/write)
        let access = 0x93 | ((dpl & 3) << 5);
        SegmentDescriptor(0x00AF_0000_0000_0000 | ((access as u64) << 40) | 0xFFFF)
    }

    /// TSS дескриптор (системный, 16 байт = 2 дескриптора)
    /// Возвращает только нижние 8 байт, верхние 8 байт должны быть добавлены отдельно
    pub fn tss(base: u64, limit: u32) -> [u64; 2] {
        let base_low = base & 0xFFFFFF;
        let base_high = (base >> 24) & 0xFF;
        
        let low = (0x0089u64 << 40) |                // Type=1001(TSS available), P=1
                 ((limit as u64) & 0xFFFF) |        // Limit low 16 bits
                 (base_low << 16) |                 // Base low 24 bits
                 ((base_high & 0xFF) << 56);        // Base high 8 bits
        
        let high = (base >> 32) & 0xFFFFFFFF;      // Base high 32 bits
        
        [low, high]
    }
}

/// GDT таблица
pub struct Gdt {
    table: [u64; 8],  // null, kernel code, kernel data, user base (заглушка), user data, user code, TSS low, TSS high
    tss: Tss,
}

static mut GDT: Gdt = Gdt {
    table: [0; 8],
    tss: Tss {
        reserved0: 0,
        rsp0: 0,
        rsp1: 0,
        rsp2: 0,
        reserved1: 0,
        ist1: 0,  // Will be set to INTERRUPT_STACK top
        ist2: 0,
        ist3: 0,
        ist4: 0,
        ist5: 0,
        ist6: 0,
        ist7: 0,
        reserved2: 0,
        reserved3: 0,
        iomap_base: 0,
    },
};

static mut GDT_DESCRIPTOR: [u8; 10] = [0; 10];

/// Инициализация GDT
pub fn init() {
    unsafe {
        // Инициализируем таблицу
        // 0x00 - Null
        GDT.table[0] = SegmentDescriptor::null().0;
        
        // 0x08 - Kernel Code (Ring 0, 64-bit)
        GDT.table[1] = SegmentDescriptor::code(0).0;
        
        // 0x10 - Kernel Data (Ring 0)
        GDT.table[2] = SegmentDescriptor::data(0).0;
        
        // 0x18 - База пары user-сегментов для SYSRET (STAR[63:48]); заглушка
        GDT.table[3] = SegmentDescriptor::null().0;
        
        // 0x20 - User Data (Ring 3)
        GDT.table[4] = SegmentDescriptor::data(3).0;
        
        // 0x28 - User Code (Ring 3, 64-bit)
        GDT.table[5] = SegmentDescriptor::code(3).0;
        
        // 0x30 - TSS (системный дескриптор, 16 байт)
        GDT.tss.iomap_base = mem::size_of::<Tss>() as u16;
        
        // Set IST1 to dedicated interrupt stack
        let int_stack_top = unsafe { &raw const INTERRUPT_STACK as *const _ as u64 + 8192 };
        GDT.tss.ist1 = int_stack_top;
        
        let tss_ptr = core::ptr::addr_of!(GDT.tss) as u64;
        let tss_limit = (mem::size_of::<Tss>() - 1) as u32;
        let tss_desc = SegmentDescriptor::tss(tss_ptr, tss_limit);
        GDT.table[6] = tss_desc[0];
        GDT.table[7] = tss_desc[1];
        
        // Загружаем GDT
        let gdt_ptr = core::ptr::addr_of!(GDT.table) as u64;
        let gdt_limit = (mem::size_of::<[u64; 8]>() - 1) as u16;
        
        // GDTR формат: [limit(2 байта)][base(8 байт)]
        let gdtr: *mut u8 = core::ptr::addr_of_mut!(GDT_DESCRIPTOR[0]);
        core::ptr::write_unaligned(gdtr as *mut u16, gdt_limit);
        core::ptr::write_unaligned(gdtr.add(2) as *mut u64, gdt_ptr);
        
        // Загружаем GDT через lgdt
        core::arch::asm!(
            "lgdt [{}]",
            in(reg) gdtr,
            options(readonly, nostack, preserves_flags)
        );
        
        // Перезагружаем Code Segment через дальний переход (ljmp)
        core::arch::asm!(
            "push {0}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            in(reg) KERNEL_CODE as u64,
            options(nostack)
        );
        
        // Загружаем Data Segment регистры
        core::arch::asm!(
            "mov ds, {0:x}",
            "mov es, {1:x}",
            "mov ss, {2:x}",
            in(reg) KERNEL_DATA,
            in(reg) KERNEL_DATA,
            in(reg) KERNEL_DATA,
            options(nostack, preserves_flags)
        );
        
        // Загружаем TSS через ltr
        core::arch::asm!(
            "ltr {0:x}",
            in(reg) TSS_SELECTOR,
            options(nostack, preserves_flags)
        );
    }
}

/// Установка kernel stack для Ring 3 syscalls (RSP0 in TSS)
/// Note: CPU caches RSP0 on ltr, so this only works if called before first ltr
/// For per-process stacks, use IST instead
pub fn set_kernel_stack(rsp0: u64) {
    unsafe {
        GDT.tss.rsp0 = rsp0;
    }
}
