extern crate alloc;

use crate::arch::x86_64::context::Context as ArchContext;
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

pub const STACK_SIZE: usize = 4096 * 4;

/// Уникальный идентификатор задачи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub u64);

/// Состояние задачи в планировщике.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running = 0,
    Ready = 1,
    Blocked = 2,
    Dead = 3,
}

impl TaskState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Running,
            1 => Self::Ready,
            2 => Self::Blocked,
            3 => Self::Dead,
            _ => unreachable!("invalid task state"),
        }
    }

    const fn as_raw(self) -> u8 {
        self as u8
    }
}

/// Адресное пространство процесса: собственный корень иерархии страниц (PML4).
///
/// Каждый процесс получает копию kernel-скелета (identity 1 GiB) в собственных
/// таблицах, поэтому пользовательские отображения (ELF, стек) не пересекаются
/// между процессами. Переключение адресных пространств происходит в
/// `switch_context` через загрузку CR3.
pub struct AddressSpace {
    /// Физический адрес корневого PML4 этого адресного пространства.
    pub root_pml4: crate::mm::PhysFrame,
}

impl AddressSpace {
    /// Создаёт новый адресное пространство, копируя текущий kernel-скелет.
    pub fn new_kernel_root(allocator: &mut crate::mm::physical::BuddyAllocator) -> AddressSpace {
        let root = crate::mm::copy_kernel_pml4(allocator)
            .expect("failed to allocate kernel PML4 skeleton");
        AddressSpace { root_pml4: root }
    }

    /// Транслировать виртуальный адрес в физический (в собственном адресном пространстве).
    pub fn translate(&self, vaddr: u64) -> Option<u64> {
        crate::mm::translate_in(self.root_pml4, vaddr)
    }

    /// Транслировать виртуальный адрес во флаги PT-записи (в собственном адресном пространстве).
    pub fn translate_flags(&self, vaddr: u64) -> Option<crate::mm::PageFlags> {
        crate::mm::translate_flags_in(self.root_pml4, vaddr)
    }
}

/// Простая `Mutex` для управления разделяемым доступом в `no_std`.
pub struct Mutex<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(inner: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(inner),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }

        MutexGuard { mutex: self }
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<'a, T> Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
    }
}

/// Стек ядра задачи с указателем на вершину.
pub struct KernelStack {
    #[allow(dead_code)]
    stack: Box<[u8; STACK_SIZE]>,
    pub top: usize,
}

impl KernelStack {
    pub fn new() -> Self {
        let mut stack = Box::new([0u8; STACK_SIZE]);
        let top = unsafe { stack.as_mut_ptr().add(STACK_SIZE) } as usize;
        let top = top & !0x0F;

        Self { stack, top }
    }
}

/// Задача ядра Hammam.
pub struct Task {
    pub id: TaskId,
    state: AtomicU8,
    pub wait_key: AtomicU64,
    pub priority: i32,
    pub context: ArchContext,
    pub kernel_stack: KernelStack,
    pub address_space: Arc<Mutex<AddressSpace>>,
    pub vruntime: u64,
    pub user_entry: u64,
    pub user_stack: u64,
}

impl Task {
    pub fn new(id: TaskId, priority: i32, address_space: Arc<Mutex<AddressSpace>>) -> Self {
        Self {
            id,
            state: AtomicU8::new(TaskState::Ready.as_raw()),
            wait_key: AtomicU64::new(0),
            priority,
            context: ArchContext::default(),
            kernel_stack: KernelStack::new(),
            address_space,
            vruntime: 0,
            user_entry: 0,
            user_stack: 0,
        }
    }

    pub fn state(&self) -> TaskState {
        TaskState::from_raw(self.state.load(Ordering::Acquire))
    }

    pub fn set_state(&self, state: TaskState) {
        self.state.store(state.as_raw(), Ordering::Release);
    }

    pub fn cas_state(
        &self,
        current: TaskState,
        next: TaskState,
    ) -> Result<TaskState, TaskState> {
        self.state
            .compare_exchange(
                current.as_raw(),
                next.as_raw(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(TaskState::from_raw)
            .map_err(TaskState::from_raw)
    }
}
