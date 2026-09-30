use crate::mm::{map_page, translate, PageFlags, PhysFrame, PHYSICAL_ALLOCATOR};
use core::sync::atomic::{AtomicU64, Ordering};

const IA32_APIC_BASE_MSR: u32 = 0x1B;
const PIT_FREQUENCY: u32 = 1_193_182;
const PIT_CHANNEL0: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;

const APIC_REG_EOI: u32 = 0x00B0;
const APIC_REG_ID: u32 = 0x0020;
const APIC_REG_ICR_LOW: u32 = 0x0300;
const APIC_REG_ICR_HIGH: u32 = 0x0310;
const APIC_REG_SVR: u32 = 0x00F0;
const APIC_REG_LVT_TIMER: u32 = 0x0320;
const APIC_REG_TIMER_INITIAL_COUNT: u32 = 0x0380;
const APIC_REG_TIMER_CURRENT_COUNT: u32 = 0x0390;
const APIC_REG_TIMER_DIVIDE: u32 = 0x03E0;

const APIC_ENABLE_BIT: u64 = 1 << 11;
const SPURIOUS_VECTOR: u32 = 0xFF;
const TIMER_VECTOR: u8 = 32;
const TIMER_MODE_PERIODIC: u32 = 1 << 17;
const TIMER_DIVIDE_CODE: u32 = 0x3; // divide by 16


pub static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Значение APIC_TIMER_INITIAL_COUNT, которым таймер реально работает.
///
/// Сохраняется в `calibrate_apic_timer`. Сам по себе reload не даёт времени без
/// знания частоты шины, но нужен для диагностики: по нему видно фактический
/// период таймера, если калибровка вдруг даст не 1 мс на тик.
static mut APIC_TIMER_RELOAD: u32 = 0;

/// Калибровка задаёт период ровно в 1 мс на тик: `reload = elapsed / 10`,
/// где `elapsed` отмерён PIT за 10 мс. Поэтому миллисекунды равны тикам
/// напрямую, без пересчёта по частоте.
pub fn uptime_millis() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

/// Текущий reload таймера (диагностика/тесты).
pub fn timer_reload() -> u32 {
    unsafe { APIC_TIMER_RELOAD }
}

/// Инкрементирует счётчик тиков. Вызывается из вектора прерывания таймера
/// ДО планировщика: если планировщик переключит контекст, счётчик уже учтён.
pub fn tick_advance() {
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);
}
static mut LOCAL_APIC_BASE: u64 = 0;
static mut IRQ_HANDLERS: [Option<fn()>; 256] = [None; 256];
const IRQ_HANDLERS_LEN: usize = 256;

/// Инициализирует локальный APIC.
///
/// 1. Читает базовый адрес из MSR IA32_APIC_BASE.
/// 2. Если нужно, включает APIC в MSR.
/// 3. Отображает физический MMIO-адрес APIC через таблицы страниц.
/// 4. Включает APIC через Spurious Interrupt Vector Register.
/// 5. Калибрует и запускает APIC Timer в периодическом режиме на ~1 мс.
pub fn init_local_apic() {
    let mut apic_msr = unsafe { read_msr(IA32_APIC_BASE_MSR) };
    let base_phys = apic_msr & 0xFFFF_F000;
    if base_phys == 0 {
        return;
    }

    if apic_msr & APIC_ENABLE_BIT == 0 {
        apic_msr |= APIC_ENABLE_BIT;
        unsafe { write_msr(IA32_APIC_BASE_MSR, apic_msr) };
    }

    let apic_virt = base_phys;
    if translate(apic_virt).is_none() {
        let mut allocator = PHYSICAL_ALLOCATOR.lock();
        let _ = unsafe {
            map_page(
                apic_virt,
                PhysFrame::new(base_phys),
                PageFlags::PRESENT | PageFlags::WRITABLE,
                &mut *allocator,
            )
        };
    }

    unsafe {
        LOCAL_APIC_BASE = apic_virt;
    }

    unsafe { crate::interrupts::set_idt_entry(TIMER_VECTOR, crate::interrupts::timer_interrupt_entry as *const () as u64, 0); }
    lapic_write(APIC_REG_SVR, 0x100 | SPURIOUS_VECTOR);
    calibrate_apic_timer();
    // Timer is now enabled (not masked) - will fire at calibrated interval
}

fn calibrate_apic_timer() {
    lapic_write(APIC_REG_TIMER_DIVIDE, TIMER_DIVIDE_CODE);
    lapic_write(APIC_REG_LVT_TIMER, TIMER_VECTOR as u32);
    lapic_write(APIC_REG_TIMER_INITIAL_COUNT, 0xFFFF_FFFF);
    pit_wait_millis(10);

    let current = lapic_read(APIC_REG_TIMER_CURRENT_COUNT);
    let elapsed = 0xFFFF_FFFFu32.wrapping_sub(current);
    let reload = if elapsed == 0 { 1 } else { elapsed / 10 };

    unsafe { APIC_TIMER_RELOAD = reload.max(1) };
    lapic_write(APIC_REG_LVT_TIMER, (TIMER_VECTOR as u32) | TIMER_MODE_PERIODIC);
    lapic_write(APIC_REG_TIMER_INITIAL_COUNT, reload.max(1));
    crate::kprintln!("[apic] timer calibrated: reload={} (~1 ms/tick), tick={} ms",
        reload.max(1), uptime_millis());
}

fn lapic_write(offset: u32, value: u32) {
    unsafe {
        let reg = apic_reg(offset);
        core::ptr::write_volatile(reg, value);
    }
}

fn lapic_read(offset: u32) -> u32 {
    unsafe { core::ptr::read_volatile(apic_reg(offset)) }
}

fn apic_reg(offset: u32) -> *mut u32 {
    unsafe { (LOCAL_APIC_BASE as *mut u8).add(offset as usize) as *mut u32 }
}

pub fn lapic_eoi() {
    lapic_write(APIC_REG_EOI, 0);
}

pub fn init_timer_only() {
    init_local_apic();
}

pub fn local_apic_id() -> u8 {
    (lapic_read(APIC_REG_ID) >> 24) as u8
}

pub fn send_icr(dest: u8, low: u32) {
    lapic_write(APIC_REG_ICR_HIGH, (dest as u32) << 24);
    lapic_write(APIC_REG_ICR_LOW, low);
    wait_for_icr();
}

fn wait_for_icr() {
    while lapic_read(APIC_REG_ICR_LOW) & (1 << 12) != 0 {}
}

pub fn register_irq_handler(vector: u8, handler: fn()) {
    if (vector as usize) < IRQ_HANDLERS_LEN {
        unsafe {
            IRQ_HANDLERS[vector as usize] = Some(handler);
        }
    }
}

pub fn handle_irq(vector: u8) {
    if let Some(handler) = unsafe { IRQ_HANDLERS[vector as usize] } {
        handler();
    }
}

fn timer_irq_handler() {
    lapic_eoi();
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);
    crate::sched::tick_now();
}

fn pit_wait_millis(milliseconds: u16) {
    let count = ((PIT_FREQUENCY * milliseconds as u32) / 1000) as u16;
    if count == 0 {
        return;
    }

    outb(PIT_COMMAND, 0x30);
    outb(PIT_CHANNEL0, (count & 0xFF) as u8);
    outb(PIT_CHANNEL0, (count >> 8) as u8);

    loop {
        if pit_read_count() == 0 {
            break;
        }
    }
}

fn pit_read_count() -> u16 {
    outb(PIT_COMMAND, 0x00);
    let lo = inb(PIT_CHANNEL0);
    let hi = inb(PIT_CHANNEL0);
    u16::from_le_bytes([lo, hi])
}

fn outb(port: u16, value: u8) {
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

unsafe fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((high as u64) << 32) | low as u64
}

unsafe fn write_msr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") low,
            in("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
}
