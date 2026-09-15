/// Multiboot2 bootloader entry point
/// Written entirely in Rust with inline assembly

const BOOT_STACK_BYTES: usize = 16 * 1024;

/// Identity page tables: PML4 + PDPT + PD (12 KiB).
#[repr(C)]
struct BootPageTables {
    pml4: [u64; 512],
    pdpt: [u64; 512],
    pd: [u64; 512],
}

/// GDT для 64-битного режима
#[repr(C, align(16))]
struct BootGDT {
    entries: [u64; 3],
}

#[repr(C, packed)]
struct GDTPointer {
    limit: u16,
    base: u64,
}

/// Boot workspace in `.data.boot` at a fixed VMA (see link.ld).
const EARLY_IDT_SIZE: usize = 4096;

#[repr(C, align(4096))]
struct BootWorkspace {
    page_tables: BootPageTables,
    gdt: BootGDT,
    gdt_pointer: GDTPointer,
    early_idt: [u8; EARLY_IDT_SIZE],
    handoff_magic: u32,
    handoff_info: u32,
    stack: [u8; BOOT_STACK_BYTES],
}

#[link_section = ".data.boot"]
#[used]
static mut BOOT_WORKSPACE: BootWorkspace = BootWorkspace {
    page_tables: BootPageTables {
        pml4: [0; 512],
        pdpt: [0; 512],
        pd: [0; 512],
    },
    gdt: BootGDT {
        entries: [
            0x0000000000000000,    // 0x00: Null descriptor
            0x00209A0000000000,    // 0x08: 64-bit code (L=1, P=1, DPL=0, S=1, Type=0xA)
            0x0000920000000000,    // 0x10: 64-bit data (P=1, DPL=0, S=1, Type=0x2)
        ],
    },
    gdt_pointer: GDTPointer {
        limit: 23,  // 3 * 8 - 1
        base: 0,    // Will be set at runtime
    },
    handoff_magic: 0,
    handoff_info: 0,
    early_idt: [0; EARLY_IDT_SIZE],
    stack: [0; BOOT_STACK_BYTES],
};

// `.data.boot` is the first section in `.data` (see link.ld / readelf -S).
// MUST match the actual VADDR of BOOT_WORKSPACE — if this triple-faults on boot,
// run «nm hammam-kernel | grep BOOT_WORKSPACE» and update this constant.
const DATA_BOOT_VADDR: u32 = 0x000000000021d000;
const BOOT_PT_ADDR: u32 = DATA_BOOT_VADDR;
const BOOT_PT_PDPT_OFF: u32 = 4096;
const BOOT_PT_PD_OFF: u32 = 8192;

const GDT_OFFSET: u32 = core::mem::size_of::<BootPageTables>() as u32;
const GDT_ADDR: u32 = DATA_BOOT_VADDR + GDT_OFFSET;
const GDT_PTR_OFFSET: u32 = GDT_OFFSET + core::mem::size_of::<BootGDT>() as u32;
const GDT_PTR_ADDR: u32 = DATA_BOOT_VADDR + GDT_PTR_OFFSET;

const EARLY_IDT_OFFSET: u32 = GDT_PTR_OFFSET + core::mem::size_of::<GDTPointer>() as u32;
const EARLY_IDT_ADDR: u32 = DATA_BOOT_VADDR + EARLY_IDT_OFFSET;
const HANDOFF_MAGIC_ADDR: u32 = EARLY_IDT_ADDR + EARLY_IDT_SIZE as u32;
const HANDOFF_INFO_ADDR: u32 = HANDOFF_MAGIC_ADDR + 4;
const BOOT_STACK_TOP: u32 = HANDOFF_INFO_ADDR + 4 + BOOT_STACK_BYTES as u32;
// IDT for early protected-mode (place in low memory to avoid being overwritten)
const IDT_ADDR: u32 = 0x00000800;

/// Multiboot2 header structure with proper alignment and tags
#[repr(C, align(8))]
pub struct Multiboot2Header {
    magic: u32,
    arch: u32,
    length: u32,
    checksum: u32,
    end_tag_type: u16,
    end_tag_flags: u16,
    end_tag_size: u32,
}

/// Static Multiboot2 header - must be in first 32KB and in loadable segment
#[link_section = ".text.boot_header"]
#[used]
pub static MULTIBOOT2_HEADER: Multiboot2Header = {
    const HEADER_LENGTH: u32 = core::mem::size_of::<Multiboot2Header>() as u32;
    const CHECKSUM: u32 =
        0u32.wrapping_sub(0xE85250D6u32.wrapping_add(0).wrapping_add(HEADER_LENGTH));

    Multiboot2Header {
        magic: 0xE85250D6,
        arch: 0,
        length: HEADER_LENGTH,
        checksum: CHECKSUM,
        end_tag_type: 0,
        end_tag_flags: 0,
        end_tag_size: 8,
    }
};

/// 32-bit entry point - called by GRUB
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.boot"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
            ".code32",

            "cli",

            // Save Multiboot2 handoff before any code clobbers EAX/EBX.
            "movl %eax, ({handoff_magic})",
            "movl %ebx, ({handoff_info})",

            // Flat data segments for rep stos and memory access under GRUB paging.
            "movw %ds, %ax",
            "movw %ax, %es",
            // Mask PICs (disable all hardware IRQs) until IDT is ready
            "movb $0xff, %al",
            "outb %al, $0x21",
            "outb %al, $0xa1",
            // Build a minimal 32-bit IDT in .data.boot (EARLY_IDT_ADDR) and load it (early)
            "leal early_exception32, %ebx",   // handler address
            "movl ${early_idt_addr}, %edi",  // IDT base (in .data.boot)
            "movl $256, %ecx",               // 256 entries
            "1:",
            "movw %bx, (%edi)",
            "movw $0x08, 2(%edi)",
            "movb $0, 4(%edi)",
            "movb $0x8E, 5(%edi)",
            "movl %ebx, %eax",
            "shrl $16, %eax",
            "movw %ax, 6(%edi)",
            "addl $8, %edi",
            "loop 1b",
            // IDTR struct is placed immediately after the table
            "movl ${early_idt_addr}, %edi",
            "addl $2048, %edi",
            "movw $2047, (%edi)",
            "movl ${early_idt_addr}, 4(%edi)",
            "lidt (%edi)",

            // Boot stack inside the loaded image.
            "movl ${boot_stack_top}, %esp",

            // Check long mode support
            "movl $0x80000000, %eax",
            "cpuid",
            "cmpl $0x80000001, %eax",
            "jb no_long_mode",

            "movl $0x80000001, %eax",
            "cpuid",
            "testl $(1 << 29), %edx",
            "jz no_long_mode",

            // Zero page tables
            "movl ${boot_pt}, %edi",
            "xorl %eax, %eax",
            "movl $3072, %ecx",
            "rep stosl",

            // PML4[0] -> PDPT  (PRESENT | WRITABLE | USER_ACCESSIBLE = 0x7)
            "movl ${boot_pt}, %edi",
            "movl ${boot_pt}, %eax",
            "addl ${pdpt_off}, %eax",
            "orl $7, %eax",
            "movl %eax, (%edi)",

            // PDPT[0] -> PD (PRESENT | WRITABLE | USER_ACCESSIBLE = 0x7)
            "movl ${boot_pt}, %eax",
            "addl ${pdpt_off}, %eax",
            "movl ${boot_pt}, %ecx",
            "addl ${pd_off}, %ecx",
            "orl $7, %ecx",
            "movl %ecx, (%eax)",

            // PD: identity-map first 1 GiB with 2 MiB pages (PRESENT | WRITABLE | HUGE_PAGE | USER_ACCESSIBLE = 0x87)
            "movl ${boot_pt}, %edi",
            "addl ${pd_off}, %edi",
            "movl $0x87, %eax",
            "movl $512, %ecx",
            "1:",
            "movl %eax, (%edi)",
            "addl $8, %edi",
            "addl $0x200000, %eax",
            "loop 1b",

            // Enable PAE
            "mov %cr4, %eax",
            "orl $0x20, %eax",
            "mov %eax, %cr4",

            // Enable long mode in EFER
            "movl $0xC0000080, %ecx",
            "rdmsr",
            "orl $0x100, %eax",
            "wrmsr",

            // Load page tables
            "movl ${boot_pt}, %eax",
            "mov %eax, %cr3",

            // Enable paging (required for long mode)
            "mov %cr0, %eax",
            "orl $0x80000000, %eax",
            "mov %eax, %cr0",

            // Setup GDT pointer base address
            "movl ${gdt_addr}, %eax",
            "movl ${gdt_ptr_addr}, %ebx",
            "movl %eax, 2(%ebx)",        // Set base (lower 32 bits)
            "movl $0, 6(%ebx)",          // Set base (upper 32 bits)

            // Load GDT for 64-bit mode  
            "lgdt (%ebx)",

            // Far jump to 64-bit code segment
            "pushl $0x08",               // Code segment selector  
            "leal start64, %eax",
            "pushl %eax",                // Offset
            "lret",                      // Pop CS:EIP and jump


            "no_long_mode:",
            "hlt",
            "jmp no_long_mode",

            ".code64",
            "start64:",
            "movw $0x10, %ax",
            "movw %ax, %ds",
            "movw %ax, %es",
            "movw %ax, %ss",
            "xorw %ax, %ax",
            "movw %ax, %fs",
            "movw %ax, %gs",
            // Загрузить magic и mbi_ptr из handoff области
            "movabs ${handoff_magic}, %rax",
            "movl (%rax), %eax",
            "movabs ${handoff_info}, %rbx",
            "movl (%rbx), %ebx",

            // Передать управление _hammam_entry
            "jmp _hammam_entry",

            // Early 32-bit exception handler
            "early_exception32:",
            "cli",
            "2:",
            "hlt",
            "jmp 2b",

            handoff_magic = const HANDOFF_MAGIC_ADDR,
            handoff_info = const HANDOFF_INFO_ADDR,
            boot_stack_top = const BOOT_STACK_TOP,
            boot_pt = const BOOT_PT_ADDR,
            pdpt_off = const BOOT_PT_PDPT_OFF,
            pd_off = const BOOT_PT_PD_OFF,
            gdt_addr = const GDT_ADDR,
            gdt_ptr_addr = const GDT_PTR_ADDR,
            early_idt_addr = const EARLY_IDT_ADDR,

            options(att_syntax)
        );
}
