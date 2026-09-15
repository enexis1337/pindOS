extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicPtr, Ordering};
pub use crate::arch::x86_64::context::{switch_context, Context as ArchContext};
use crate::drivers::serial::SpinMutex;
use crate::sched::task::{Task, TaskState};
use crate::kprintln;

pub mod task;

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
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            run_queue: BTreeMap::new(),
            current: None,
            min_vruntime: 0,
            current_ticks: 0,
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

    pub fn pick_next(&mut self) -> Option<Arc<Task>> {
        if self.run_queue.is_empty() {
            return None;
        }

        if let Some(current) = self.current.take() {
            if current.state != TaskState::Dead {
                let mut key = current.vruntime.saturating_add(1);
                while self.run_queue.contains_key(&key) {
                    key = key.saturating_add(1);
                }
                self.run_queue.insert(key, current);
            }
        }

        let (&next_vruntime, next_task) = self.run_queue.iter().next()?;
        let next = next_task.clone();
        self.run_queue.remove(&next_vruntime);
        self.min_vruntime = next_vruntime;
        self.current = Some(next.clone());
        Some(next)
    }

    fn schedule_locked(&mut self) -> Option<(*mut Task, *const Task)> {
        let current = self.current.as_ref()?.clone();
        let next = self.pick_next()?;
        if Arc::ptr_eq(&current, &next) {
            return None;
        }

        unsafe {
            let current_ptr = Arc::as_ptr(&current) as *mut Task;
            let next_ptr = Arc::as_ptr(&next) as *const Task as *mut Task;
            (*current_ptr).state = TaskState::Ready;
            (*next_ptr).state = TaskState::Running;
            // Update CURRENT_TASK for trampoline
            crate::sched::set_current_task(next_ptr);
            Some((current_ptr, next_ptr))
        }
    }

    pub fn schedule(&mut self) -> Option<(*mut Task, *const Task)> {
        self.schedule_locked()
    }

    pub fn tick(&mut self) -> Option<(*mut Task, *const Task)> {
        let current = self.current.as_ref()?;
        unsafe {
            let current_ptr = Arc::as_ptr(current) as *mut Task;
            let increment = 1_000_000 / Self::weight(current.priority);
            (*current_ptr).vruntime = (*current_ptr).vruntime.saturating_add(increment);
        }

        self.current_ticks = self.current_ticks.saturating_add(1);
        if self.current_ticks >= QUANTUM_TICKS {
            self.current_ticks = 0;
            self.schedule_locked()
        } else {
            None
        }
    }
}

pub static SCHEDULER: SpinMutex<Scheduler> = SpinMutex::new(Scheduler::new());

static mut MAIN_CONTEXT: ArchContext = ArchContext::new();

/// Переключает ядро на целевую задачу: обновляет kernel-стек (SYSCALL), TSS.rsp0
/// и CR3, затем выполняет низкоуровневый `switch_context`.
///
/// Возвращается, когда задача позже снова получит CPU (лежит в `from` контексте).
///
/// # Safety
/// `from` — текущая задача (её контекст будет сохранён), `to` — целевая задача.
/// Обе должны иметь валидные kernel-стеки и адресные пространства.
pub unsafe fn switch_to_task(from: *mut Task, to: *const Task) {
    unsafe {
        let to_task = &*to;
        // SYSCALL entry и аппаратные прерывания (TSS.rsp0) используют kernel-стек
        // именно той задачи, которая сейчас исполняется.
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
            unsafe {
                let mut line = alloc::string::String::new();
                for i in 0..(2*N+2) {
                    use alloc::fmt::Write;
                    let _ = write!(line, " {:x}", *((base + i*8) as *const u64));
                }
                crate::kprintln!("[ctx]  stack:{}", line);
            }
        }

        switch_context(from_ctx, to_ctx);
    }
}

pub fn create_kernel_thread(main: extern "C" fn() -> !) -> Arc<Task> {
    use alloc::sync::Arc;
    use crate::sched::task::{AddressSpace, Mutex};

    let address_space = {
        let mut allocator = crate::mm::PHYSICAL_ALLOCATOR.lock();
        Arc::new(Mutex::new(AddressSpace::new_kernel_root(&mut allocator)))
    };
    let task = Arc::new(Task::new(task::TaskId(0), 1, address_space));

    let stack_top = task.kernel_stack.top;
    let stack_ptr = (stack_top - core::mem::size_of::<u64>()) as *mut u64;
    unsafe { *stack_ptr = main as u64 };
    unsafe {
        let task_ptr = Arc::as_ptr(&task) as *mut Task;
        (*task_ptr).context.rsp = stack_ptr as u64;
        (*task_ptr).context.cr3 = crate::mm::active_pml4().start_address;
        (*task_ptr).state = TaskState::Ready;
    }

    task
}

pub fn exit_current() -> ! {
    let return_pair = {
        let mut scheduler = SCHEDULER.lock();
        if let Some(current) = scheduler.current.as_ref() {
            unsafe {
                let current_task = Arc::as_ptr(current) as *mut Task;
                (*current_task).state = TaskState::Dead;
            }
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
        // Set CURRENT_TASK for trampoline
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
        // Перед первым входом в userspace настроим kernel-стек и адресное пространство.
        crate::arch::gdt::set_kernel_stack(kernel_stack_top as u64);
        crate::arch::x86_64::syscall::set_kernel_stack(kernel_stack_top as u64);
        crate::mm::set_cr3(crate::mm::PhysFrame::new(cr3));
        switch_context(&raw mut MAIN_CONTEXT as *mut ArchContext, to)
    }
}

pub fn schedule_now() {
    let pair = {
        let mut scheduler = SCHEDULER.lock();
        kprintln!("[sched] schedule_now: current={:?}, run_queue_len={}",
            scheduler.current.as_ref().map(|t| t.id.0), scheduler.run_queue.len());
        scheduler.schedule()
    };
    if let Some((from, to)) = pair {
        kprintln!("[sched] switching context");
        unsafe { switch_to_task(from, to) }
    } else {
        kprintln!("[sched] no switch needed");
    }
}

pub fn tick_now() {
    let pair = {
        let mut scheduler = SCHEDULER.lock();
        if let Some(current) = scheduler.current.as_ref() {
            unsafe {
                let current_ptr = Arc::as_ptr(current) as *mut Task;
                let increment = 1_000_000 / Scheduler::weight(current.priority);
                (*current_ptr).vruntime = (*current_ptr).vruntime.saturating_add(increment);
            }
        }
        scheduler.current_ticks = scheduler.current_ticks.saturating_add(1);
        if scheduler.current_ticks >= QUANTUM_TICKS {
            scheduler.current_ticks = 0;
            scheduler.schedule()
        } else {
            None
        }
    };
    if let Some((from, to)) = pair {
        kprintln!("[tick] context switch");
        unsafe { switch_to_task(from, to) }
    }
}

#[no_mangle]
pub extern "C" fn tick_now_debug() {
    unsafe { crate::drivers::serial::SERIAL.get().write_byte(b'.'); }
    tick_now();
}

pub fn yield_now() {
    schedule_now();
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::sync::Arc;
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
            switch_context(&mut MAIN_CONTEXT as *mut ArchContext, to);
        }
    }
}
