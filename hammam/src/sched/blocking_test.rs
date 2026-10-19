//! Kernel-only smoke test; enabled explicitly with --features sched-test.
use super::*;
use core::sync::atomic::{AtomicU64, AtomicUsize};

const IDLE_A: u64 = TEST_KEY + 1;
const IDLE_B: u64 = TEST_KEY + 2;
static EVENT: AtomicBool = AtomicBool::new(false);
static IDLE_EVENT: AtomicBool = AtomicBool::new(false);
static DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);
static IDLE_HITS: AtomicUsize = AtomicUsize::new(0);
static START: AtomicU64 = AtomicU64::new(0);

pub fn idle_entered() {
    IDLE_HITS.fetch_add(1, Ordering::Relaxed);
}

/// IRQ only: atomics and wake, no printing, allocation or scheduler lock.
pub fn on_tick(now: u64) {
    if now >= DEADLINE.load(Ordering::Acquire)
        && IDLE_HITS.load(Ordering::Acquire) != 0
        && !IDLE_EVENT.swap(true, Ordering::AcqRel)
    {
        wake(IDLE_A);
        wake(IDLE_B);
    }
}

fn check_error_paths() {
    let flags = unsafe { save_and_disable_interrupts() };
    let current = SCHEDULER.lock().current.as_ref().unwrap().clone();
    let references = Arc::strong_count(&current);
    assert_eq!(block_current(0, || false), Err(BlockError::InvalidKey));
    for slot in &BLOCKED {
        assert!(slot.load(Ordering::Acquire).is_null());
        slot.store(Arc::into_raw(current.clone()) as *mut Task, Ordering::Release);
    }
    assert_eq!(block_current(TEST_KEY, || false), Err(BlockError::NoSlots));
    assert_eq!(current.state(), TaskState::Running);
    assert_eq!(current.wait_key.load(Ordering::Acquire), 0);
    for slot in &BLOCKED {
        drop(unsafe { Arc::from_raw(slot.swap(core::ptr::null_mut(), Ordering::AcqRel)) });
    }
    assert_eq!(Arc::strong_count(&current), references);
    let after = unsafe { save_and_disable_interrupts() };
    assert_eq!(after & (1 << 9), 0);
    unsafe { restore_interrupts(flags) };
    kprintln!("[block-test] PASS invalid key / full slots: state, IF and Arc count unchanged");

    let dead = create_kernel_thread(waker);
    dead.wait_key.store(TEST_KEY, Ordering::Release);
    dead.set_state(TaskState::Dead);
    BLOCKED[0].store(Arc::into_raw(dead.clone()) as *mut Task, Ordering::Release);
    wake(TEST_KEY);
    assert_eq!(Arc::strong_count(&dead), 2);
    assert!(!BLOCKED[0].load(Ordering::Acquire).is_null());
    {
        let mut scheduler = SCHEDULER.lock();
        scheduler.drain_blocked();
        assert!(scheduler.run_queue.values().all(|task| !Arc::ptr_eq(task, &dead)));
    }
    assert_eq!(Arc::strong_count(&dead), 1);
    assert_eq!(dead.wait_key.load(Ordering::Acquire), 0);
    assert!(BLOCKED[0].load(Ordering::Acquire).is_null());
    kprintln!("[block-test] PASS Dead slot: IRQ deferred cleanup, scheduler released Arc");
}

extern "C" fn waiter() -> ! {
    check_error_paths();
    EVENT.store(true, Ordering::Release);
    wake(TEST_KEY);
    let slept = block_current(TEST_KEY, || EVENT.load(Ordering::Acquire)).unwrap();
    assert!(!slept);
    kprintln!("[block-test] PASS wake before sleep: condition true, no switch");

    EVENT.store(false, Ordering::Release);
    START.store(crate::arch::x86_64::apic::uptime_millis(), Ordering::Release);
    kprintln!("[block-test] A: blocking, waiting for B (~100 ticks)");
    // Exercise both IF=1 restoration here and IF=0 restoration below.
    unsafe { core::arch::asm!("sti", options(nostack)); }
    while !EVENT.load(Ordering::Acquire) {
        assert!(block_current(TEST_KEY, || EVENT.load(Ordering::Acquire)).unwrap());
    }
    let flags = unsafe { save_and_disable_interrupts() };
    assert_ne!(flags & (1 << 9), 0);
    let elapsed = crate::arch::x86_64::apic::uptime_millis() - START.load(Ordering::Acquire);
    assert!(elapsed >= 100);
    kprintln!("[block-test] PASS wake during sleep: A resumed after {} ticks, IF restored", elapsed);

    DEADLINE.store(crate::arch::x86_64::apic::uptime_millis() + 100, Ordering::Release);
    kprintln!("[block-test] A: blocking for idle/IRQ scenario");
    while !IDLE_EVENT.load(Ordering::Acquire) {
        block_current(IDLE_A, || IDLE_EVENT.load(Ordering::Acquire)).unwrap();
    }
    let flags = unsafe { save_and_disable_interrupts() };
    assert_eq!(flags & (1 << 9), 0);
    assert!(IDLE_HITS.load(Ordering::Acquire) != 0);
    assert!(BLOCKED.iter().all(|slot| slot.load(Ordering::Acquire).is_null()));
    kprintln!("[block-test] PASS all tasks blocked: idle ran, timer IRQ woke A and B, IF=0 restored");
    kprintln!("[block-test] ALL PASS");
    exit_current();
}

extern "C" fn waker() -> ! {
    unsafe { core::arch::asm!("sti", options(nostack)); }
    while crate::arch::x86_64::apic::uptime_millis() - START.load(Ordering::Acquire) < 100 {
        unsafe { core::arch::asm!("hlt", options(nostack)); }
    }
    kprintln!("[block-test] B: 100 ticks elapsed, wake(TEST_KEY)");
    EVENT.store(true, Ordering::Release);
    wake(TEST_KEY);
    schedule_now();

    kprintln!("[block-test] B: blocking too; only idle remains runnable");
    while !IDLE_EVENT.load(Ordering::Acquire) {
        block_current(IDLE_B, || IDLE_EVENT.load(Ordering::Acquire)).unwrap();
    }
    kprintln!("[block-test] B: resumed from IRQ wake");
    exit_current();
}

pub fn run() -> ! {
    let a = create_kernel_thread(waiter);
    let b = create_kernel_thread(waker);
    {
        let mut scheduler = SCHEDULER.lock();
        scheduler.add_task(a);
        scheduler.add_task(b);
    }
    kprintln!("[block-test] kernel-only; userspace disabled");
    start_scheduler();
    panic!("kernel-only scheduler returned");
}
