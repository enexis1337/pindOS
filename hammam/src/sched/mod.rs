extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
pub use crate::arch::x86_64::context::{switch_context, Context as ArchContext};
use crate::drivers::serial::SpinMutex;
use crate::sched::task::{Task, TaskState};
use crate::kprintln;

pub mod task;

/// Ключ ожидания консольного RX.
pub const WAIT_CONSOLE: u64 = 1;
/// Ключ ожидания дочернего процесса.
pub const fn wait_child(pid: u32) -> u64 {
    (1u64 << 32) | pid as u64
}

const MAX_BLOCKED_TASKS: usize = 64;
static BLOCKED: [AtomicPtr<Task>; MAX_BLOCKED_TASKS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_BLOCKED_TASKS];
static RESCHEDULE_PENDING: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "sched-test")]
pub const TEST_KEY: u64 = 0x5445_5354;

/// Current running task (set by scheduler during context switch)
static CURRENT_TASK: AtomicPtr<Task> = AtomicPtr::new(core::ptr::null_mut());

pub fn set_current_task(task: *mut Task) {
    CURRENT_TASK.store(task, Ordering::Release);
}

pub fn get_current_task() -> Option<&'static mut Task> {
    let ptr = CURRENT_TASK.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &mut *ptr })
    }
}

const QUANTUM_TICKS: u8 = 10;

pub struct Scheduler {
    pub run_queue: BTreeMap<u64, Arc<Task>>,
    pub current: Option<Arc<Task>>,
    pub min_vruntime: u64,
    current_ticks: u8,
    idle: Option<Arc<Task>>,
    // Освобождаем стек умершей задачи только после переключения с него.
    retired: Option<Arc<Task>>,
    // IRQ отдаёт current idle без вставки в дерево; idle заберёт эту ссылку.
    deferred_current: Option<Arc<Task>>,
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            run_queue: BTreeMap::new(),
            current: None,
            min_vruntime: 0,
            current_ticks: 0,
            idle: None,
            retired: None,
            deferred_current: None,
        }
    }

    fn weight(priority: i32) -> u64 {
        let priority = priority.max(1).min(1024) as u64;
        1024 / priority
    }

    fn insert_task_into_queue(&mut self, task: Arc<Task>) {
        let mut vruntime = self.min_vruntime;
        if let Some(current) = self.current.as_ref() {
            vruntime = vruntime.max(current.vruntime.saturating_add(1));
        }

        let mut key = vruntime;
        while self.run_queue.contains_key(&key) {
            key = key.saturating_add(1);
        }

        if key != task.vruntime {
            unsafe {
                let task_ptr = Arc::as_ptr(&task) as *mut Task;
                (*task_ptr).vruntime = key;
            }
        }

        self.run_queue.insert(key, task);
    }

    pub fn add_task(&mut self, task: Arc<Task>) {
        if self.current.is_none() {
            self.current = Some(task);
        } else {
            self.insert_task_into_queue(task);
        }
    }

    pub fn set_idle(&mut self, task: Arc<Task>) {
        self.idle = Some(task);
    }

    /// Переносит разбуженные задачи из IRQ-safe слотов в обычную очередь.
    fn drain_blocked(&mut self) {
        for slot in &BLOCKED {
            let ptr = slot.load(Ordering::Acquire);
            if ptr.is_null() {
                continue;
            }
            let task = unsafe { &*ptr };
            let state = task.state();
            if !matches!(state, TaskState::Ready | TaskState::Dead) {
                continue;
            }
            if slot
                .compare_exchange(ptr, core::ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            let task = unsafe { Arc::from_raw(ptr) };
            task.wait_key.store(0, Ordering::Release);
            if task.state() == TaskState::Ready {
                self.enqueue_ready(&task);
            }
        }
        RESCHEDULE_PENDING.store(false, Ordering::Release);
    }

    pub fn pick_next(&mut self) -> Option<Arc<Task>> {
        self.pick_next_impl(false)
    }

    /// Обычный контекст, IF=0. IRQ никогда не меняет структуру дерева.
    fn pick_next_impl(&mut self, exclude_current: bool) -> Option<Arc<Task>> {
        let current = self.current.as_ref()?.clone();
        let runnable = matches!(current.state(), TaskState::Ready | TaskState::Running);
        let is_idle = self.is_idle(&current);
        if !exclude_current && runnable && !is_idle {
            self.enqueue_ready(&current);
        }

        let candidate = self.run_queue.iter()
            .filter(|(_, task)| task.state() == TaskState::Ready || task.state() == TaskState::Running)
            .min_by_key(|(_, task)| task.vruntime)
            .map(|(&key, task)| (key, task.vruntime));
        let next = if let Some((key, vruntime)) = candidate {
            self.min_vruntime = vruntime;
            self.run_queue.remove(&key).unwrap()
        } else if runnable && !is_idle {
            return None;
        } else {
            self.idle.as_ref()?.clone()
        };

        if exclude_current && runnable && !is_idle {
            self.enqueue_ready(&current);
        }
        self.current = Some(next.clone());
        Some(next)
    }

    fn is_idle(&self, task: &Arc<Task>) -> bool {
        self.idle.as_ref().is_some_and(|idle| Arc::ptr_eq(idle, task))
    }

    /// Кладёт задачу в run_queue с минимальным свободным ключом от `vruntime`.
    fn enqueue_ready(&mut self, task: &Arc<Task>) {
        if !matches!(task.state(), TaskState::Ready | TaskState::Running) || self.is_idle(task) {
            return;
        }
        let mut key = task.vruntime.saturating_add(1);
        while self.run_queue.contains_key(&key) {
            key = key.checked_add(1).expect("run queue key overflow");
        }
        self.run_queue.insert(key, task.clone());
    }

    fn schedule_locked_with(&mut self, yield_path: bool) -> Option<(*mut Task, *const Task)> {
        // Предыдущая умершая задача уже не исполняется на своём стеке.
        self.retired.take();
        if let Some(task) = self.deferred_current.take() {
            self.enqueue_ready(&task);
        }
        self.drain_blocked();
        let current = self.current.as_ref()?.clone();
        let next = self.pick_next_impl(yield_path)?;
        self.prepare_switch(current, next)
    }

    fn prepare_switch(&mut self, current: Arc<Task>, next: Arc<Task>) -> Option<(*mut Task, *const Task)> {
        if Arc::ptr_eq(&current, &next) {
            return None;
        }
        current.cas_state(TaskState::Running, TaskState::Ready).ok();
        next.set_state(TaskState::Running);
        let from = Arc::as_ptr(&current) as *mut Task;
        let to = Arc::as_ptr(&next);
        set_current_task(to as *mut Task);
        if current.state() == TaskState::Dead {
            self.retired = Some(current);
        }
        Some((from, to))
    }

    pub fn schedule(&mut self) -> Option<(*mut Task, *const Task)> {
        self.schedule_locked_with(false)
    }

    pub fn schedule_yield(&mut self) -> Option<(*mut Task, *const Task)> {
        self.schedule_locked_with(true)
    }

    /// IRQ-путь: заменяем Arc в уже выделенном узле, без insert/remove/drop
    /// последней ссылки. Ключ узла — позиция, поэтому оба пути выбора
    /// сравнивают актуальные vruntime (O(n), до смены структуры run_queue).
    pub fn tick(&mut self) -> Option<(*mut Task, *const Task)> {
        let current = self.current.as_ref()?.clone();
        if self.is_idle(&current) {
            return None;
        }
        if RESCHEDULE_PENDING.load(Ordering::Acquire) {
            let idle = self.idle.as_ref()?.clone();
            self.deferred_current = Some(current.clone());
            self.current = Some(idle.clone());
            return self.prepare_switch(current, idle);
        }
        unsafe {
            let ptr = Arc::as_ptr(&current) as *mut Task;
            let increment = 1_000_000 / Self::weight(current.priority);
            (*ptr).vruntime = (*ptr).vruntime.saturating_add(increment);
        }
        self.current_ticks = self.current_ticks.saturating_add(1);
        if self.current_ticks < QUANTUM_TICKS {
            return None;
        }
        self.current_ticks = 0;
        let candidate = self.run_queue.iter()
            .filter(|(_, task)| task.state() == TaskState::Ready)
            .min_by_key(|(_, task)| task.vruntime)
            .map(|(&key, task)| (key, task.vruntime));
        let (key, vruntime) = candidate?;
        if current.vruntime < vruntime {
            return None;
        }
        let next = core::mem::replace(self.run_queue.get_mut(&key).unwrap(), current.clone());
        self.current = Some(next.clone());
        self.min_vruntime = vruntime;
        self.prepare_switch(current, next)
    }
}

pub static SCHEDULER: SpinMutex<Scheduler> = SpinMutex::new(Scheduler::new());

/// Пробуждает задачи без scheduler lock и без аллокации.
pub fn wake(wait_key: u64) {
    // UP: cli защищает время жизни указателей от drain_blocked. Для SMP
    // потребуются per-CPU очереди или отдельный протокол reclamation.
    let flags = unsafe { save_and_disable_interrupts() };
    if wait_key == 0 {
        unsafe { restore_interrupts(flags) };
        return;
    }
    for slot in &BLOCKED {
        let ptr = slot.load(Ordering::Acquire);
        if ptr.is_null() {
            continue;
        }
        let task = unsafe { &*ptr };
        if task.state() == TaskState::Dead {
            RESCHEDULE_PENDING.store(true, Ordering::Release);
            continue;
        }
        if task.wait_key.load(Ordering::Acquire) != wait_key {
            continue;
        }
        if task.cas_state(TaskState::Blocked, TaskState::Ready).is_ok() {
            RESCHEDULE_PENDING.store(true, Ordering::Release);
        }
    }
    unsafe { restore_interrupts(flags) };
}

/// Ошибки ожидания не меняют состояние текущей задачи.
#[derive(Debug, PartialEq, Eq)]
pub enum BlockError {
    InvalidKey,
    NoCurrentTask,
    NoIdleTask,
    NoSlots,
}

/// `condition` означает «событие уже произошло» и не должна спать или
/// удерживать lock после возврата. Вызывающий перепроверяет условие в цикле.
pub fn block_current<F>(wait_key: u64, condition: F) -> Result<bool, BlockError>
where
    F: FnOnce() -> bool,
{
    let flags = unsafe { save_and_disable_interrupts() };
    let result = block_current_disabled(wait_key, condition);
    unsafe { restore_interrupts(flags) };
    result
}

fn block_current_disabled<F>(wait_key: u64, condition: F) -> Result<bool, BlockError>
where
    F: FnOnce() -> bool,
{
    if wait_key == 0 {
        return Err(BlockError::InvalidKey);
    }
    if condition() {
        return Ok(false);
    }
    let current = {
        let scheduler = SCHEDULER.lock();
        if scheduler.idle.is_none() {
            return Err(BlockError::NoIdleTask);
        }
        let task = scheduler.current.as_ref().ok_or(BlockError::NoCurrentTask)?;
        if scheduler.is_idle(task) {
            return Err(BlockError::NoCurrentTask);
        }
        task.clone()
    };
    let slot = BLOCKED.iter()
        .find(|slot| slot.load(Ordering::Acquire).is_null())
        .ok_or(BlockError::NoSlots)?;
    current.wait_key.store(wait_key, Ordering::Release);
    current.set_state(TaskState::Blocked);
    // Слот владеет отдельной strong reference до обычноконтекстного drain.
    slot.store(Arc::into_raw(current.clone()) as *mut Task, Ordering::Release);
    // Не оставлять self-owning Arc на спящем стеке: Dead-задача может уже
    // никогда не вернуться, чтобы выполнить его деструктор.
    drop(current);
    let pair = SCHEDULER.lock().schedule();
    if let Some((from, to)) = pair {
        unsafe { switch_to_task(from, to) };
    } else {
        // Idle зарегистрирована: Blocked current никогда не кандидат на CPU.
        unreachable!("blocked task has no switch target");
    }
    Ok(true)
}

unsafe fn save_and_disable_interrupts() -> u64 {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags); }
    flags
}

unsafe fn restore_interrupts(flags: u64) {
    if flags & (1 << 9) != 0 {
        unsafe { core::arch::asm!("sti", options(nostack)); }
    }
}

extern "C" fn idle_main() -> ! {
    loop {
        let flags = unsafe { save_and_disable_interrupts() };
        schedule_now();
        #[cfg(feature = "sched-test")]
        blocking_test::idle_entered();
        // Проверка pending и HLT непрерываемы; STI откладывает IRQ до HLT.
        unsafe { core::arch::asm!("sti; hlt; cli", options(nostack)); }
        unsafe { restore_interrupts(flags) };
    }
}

pub fn init_idle() {
    let idle = create_kernel_thread(idle_main);
    SCHEDULER.lock().set_idle(idle);
}

static mut MAIN_CONTEXT: ArchContext = ArchContext::new();

/// Переключает ядро на целевую задачу: обновляет kernel-стек (SYSCALL), TSS.rsp0
/// и CR3, затем выполняет низкоуровневый `switch_context`.
pub unsafe fn switch_to_task(from: *mut Task, to: *const Task) {
    unsafe {
        let to_task = &*to;
        crate::arch::gdt::set_kernel_stack(to_task.kernel_stack.top as u64);
        crate::arch::x86_64::syscall::set_kernel_stack(to_task.kernel_stack.top as u64);

        let from_ctx = &mut (*from).context as *mut ArchContext;
        let to_ctx = &to_task.context as *const ArchContext;

        if crate::arch::x86_64::syscall::SC_DEBUG {
            const N: usize = 6;
            let base = (to_task.context.rsp as usize / 8).saturating_sub(N) * 8;
            crate::kprintln!("[ctx] f{}->t{} trsp={:#x} tcr3={:#x} top={:#x} prv={:#x}",
                (*from).id.0, to_task.id.0,
                to_task.context.rsp, to_task.context.cr3,
                to_task.kernel_stack.top,
                *((to_task.context.rsp as *const u64)),
            );
            crate::kprint!("[ctx]  stack:");
            for i in 0..(2*N+2) {
                crate::kprint!(" {:x}", *((base + i*8) as *const u64));
            }
            crate::kprintln!();
        }

        switch_context(from_ctx, to_ctx);
    }
}

pub fn create_kernel_thread(main: extern "C" fn() -> !) -> Arc<Task> {
    use crate::sched::task::{AddressSpace, Mutex};

    let address_space = {
        let mut allocator = crate::mm::PHYSICAL_ALLOCATOR.lock();
        Arc::new(Mutex::new(AddressSpace::new_kernel_root(&mut allocator)))
    };
    let task = Arc::new(Task::new(task::TaskId(0), 1, address_space));

    let stack_top = task.kernel_stack.top;
    // После RET на входе в extern "C" функцию RSP % 16 должен быть равен 8.
    let stack_ptr = (stack_top - 2 * core::mem::size_of::<u64>()) as *mut u64;
    unsafe { *stack_ptr = main as *const () as u64 };
    unsafe {
        let task_ptr = Arc::as_ptr(&task) as *mut Task;
        (*task_ptr).context.rsp = stack_ptr as u64;
        (*task_ptr).context.cr3 = crate::mm::active_pml4().start_address;
        (*task_ptr).set_state(TaskState::Ready);
    }

    task
}

pub fn exit_current() -> ! {
    unsafe { save_and_disable_interrupts() };
    let return_pair = {
        let mut scheduler = SCHEDULER.lock();
        if let Some(current) = scheduler.current.as_ref() {
            current.set_state(TaskState::Dead);
        }
        scheduler.schedule()
    };

    if let Some((from, to)) = return_pair {
        unsafe { switch_to_task(from, to) }
    }

    kprintln!("[sched] no more tasks, halting");
    loop {
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags)); }
    }
}

pub fn start_scheduler() {
    let (kernel_stack_top, cr3, to) = {
        let scheduler = SCHEDULER.lock();
        let current = scheduler.current.as_ref().unwrap();
        let current_ptr = Arc::as_ptr(current) as *mut Task;
        current.set_state(TaskState::Running);
        crate::sched::set_current_task(current_ptr);
        unsafe {
            (
                (*current_ptr).kernel_stack.top,
                (*current_ptr).context.cr3,
                &(*current_ptr).context as *const ArchContext,
            )
        }
    };

    unsafe {
        crate::arch::gdt::set_kernel_stack(kernel_stack_top as u64);
        crate::arch::x86_64::syscall::set_kernel_stack(kernel_stack_top as u64);
        crate::mm::set_cr3(crate::mm::PhysFrame::new(cr3));
        switch_context(&raw mut MAIN_CONTEXT as *mut ArchContext, to)
    }
}

/// Подробный лог планировщика. Выключен: при двух и более процессах он даёт
/// тысячи строк в минуту и забивает COM1, когда появится shell.
pub const SCHED_DEBUG: bool = false;

pub fn schedule_now() {
    let flags = unsafe { save_and_disable_interrupts() };
    let pair = {
        let mut scheduler = SCHEDULER.lock();
        if SCHED_DEBUG {
            kprintln!("[sched] schedule_now: current={:?}, run_queue_len={}",
                scheduler.current.as_ref().map(|t| t.id.0), scheduler.run_queue.len());
        }
        scheduler.schedule_yield()
    };
    if let Some((from, to)) = pair {
        if SCHED_DEBUG { kprintln!("[sched] switching context"); }
        unsafe { switch_to_task(from, to) }
    } else if SCHED_DEBUG {
        kprintln!("[sched] no switch needed");
    }
    unsafe { restore_interrupts(flags) };
}

pub fn tick_now() {
    let pair = SCHEDULER.lock().tick();
    if let Some((from, to)) = pair {
        if SCHED_DEBUG { kprintln!("[tick] context switch"); }
        unsafe { switch_to_task(from, to) }
    }
}

#[no_mangle]
pub extern "C" fn tick_now_debug(interrupted_cs: u64) {
    // Не вытесняем kernel-код: он может держать heap/physical/scheduler lock.
    // После IRET idle сам обработает wake; userspace можно вытеснять сразу.
    if interrupted_cs & 3 == 0 {
        return;
    }
    if SCHED_DEBUG {
        unsafe { crate::drivers::serial::SERIAL.get().write_byte(b'.'); }
    }
    tick_now();
}

pub fn yield_now() {
    schedule_now();
}

#[cfg(feature = "sched-test")]
pub mod blocking_test;

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use crate::kprintln;

    extern "C" fn task_a() -> ! {
        kprintln!("[sched] task A");
        yield_now();
        kprintln!("[sched] task A done");
        exit_current();
    }

    extern "C" fn task_b() -> ! {
        kprintln!("[sched] task B");
        yield_now();
        kprintln!("[sched] task B done");
        exit_current();
    }

    extern "C" fn task_c() -> ! {
        kprintln!("[sched] task C");
        yield_now();
        kprintln!("[sched] task C done");
        exit_current();
    }

    #[test]
    fn round_robin_kernel_threads() {
        let t1 = create_kernel_thread(task_a);
        let t2 = create_kernel_thread(task_b);
        let t3 = create_kernel_thread(task_c);

        {
            let mut scheduler = SCHEDULER.lock();
            scheduler.add_task(t1);
            scheduler.add_task(t2);
            scheduler.add_task(t3);
        }

        let to = {
            let scheduler = SCHEDULER.lock();
            &scheduler.current.as_ref().unwrap().context as *const ArchContext
        };
        unsafe {
            switch_context(&raw mut MAIN_CONTEXT as *mut ArchContext, to);
        }
    }
}
