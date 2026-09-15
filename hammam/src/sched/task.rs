extern crate alloc;

use crate::arch::x86_64::context::Context as ArchContext;
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub const STACK_SIZE: usize = 4096 * 4;

/// Уникальный идентификатор задачи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub u64);

/// Состояние задачи в планировщике.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Ready,
    Blocked,
    Dead,
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
    pub state: TaskState,
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
            state: TaskState::Ready,
            priority,
            context: ArchContext::default(),
            kernel_stack: KernelStack::new(),
            address_space,
            vruntime: 0,
            user_entry: 0,
            user_stack: 0,
        }
    }
}
