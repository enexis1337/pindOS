use core::alloc::{GlobalAlloc, Layout};
use crate::drivers::serial::SpinMutex;

/// Размер кучи ядра
pub const HEAP_SIZE: usize = 16 * 1024 * 1024; // 16 MiB

/// Статическая область памяти в секции BSS, используемая в качестве кучи ядра.
static mut HEAP_SPACE: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// Узел связного списка свободных блоков памяти кучи (инлайн-хранение).
struct Hole {
    size: usize,
    next: Option<*mut Hole>,
}

/// Простой кучевой аллокатор на базе связного списка свободных блоков.
pub struct LinkedHeapAllocator {
    head: Hole,
}

// SAFETY: LinkedHeapAllocator осуществляет операции с сырыми указателями,
// но так как доступ к нему обернут в SpinMutex в global_allocator, это безопасно.
unsafe impl Send for LinkedHeapAllocator {}

impl LinkedHeapAllocator {
    /// Создает пустой экземпляр аллокатора.
    pub const fn new() -> Self {
        Self {
            head: Hole {
                size: 0,
                next: None,
            },
        }
    }

    /// Инициализирует аллокатор регионом свободной памяти.
    ///
    /// # Safety
    /// Функция должна быть вызвана один раз при старте кучи.
    pub unsafe fn init(&mut self, start_addr: usize, size: usize) {
        let hole_ptr = start_addr as *mut Hole;
        // SAFETY: Память в выделенном статическом регионе свободна и выровнена.
        unsafe {
            (*hole_ptr).size = size;
            (*hole_ptr).next = None;
            self.head.next = Some(hole_ptr);
        }
    }

    /// Выделяет блок памяти с учетом размера и выравнивания.
    ///
    /// # Safety
    /// Должно вызываться только внутри защищенного контекста (SpinMutex).
    unsafe fn alloc_raw(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        let size = core::cmp::max(size, core::mem::size_of::<Hole>());
        let align = core::cmp::max(align, core::mem::align_of::<Hole>());
        let size = (size + align - 1) & !(align - 1);

        let mut prev: *mut Hole = &mut self.head;
        
        // SAFETY: Обход списка свободных блоков с помощью сырых указателей.
        unsafe {
            while let Some(curr) = (*prev).next {
                let curr_addr = curr as usize;
                let aligned_addr = (curr_addr + align - 1) & !(align - 1);
                let padding = aligned_addr - curr_addr;
                let required_size = size + padding;

                if (*curr).size >= required_size {
                    let next_hole = (*curr).next;
                    let remaining_size = (*curr).size - required_size;

                    if remaining_size >= core::mem::size_of::<Hole>() {
                        // Разрезаем блок на используемый и остаток свободной памяти
                        let new_hole_addr = aligned_addr + size;
                        let new_hole_ptr = new_hole_addr as *mut Hole;
                        (*new_hole_ptr).size = remaining_size;
                        (*new_hole_ptr).next = next_hole;
                        (*prev).next = Some(new_hole_ptr);
                    } else {
                        // Забираем блок целиком, так как остаток слишком мал
                        (*prev).next = next_hole;
                    }
                    return Some(aligned_addr as *mut u8);
                }
                prev = curr;
            }
        }
        None
    }

    /// Возвращает блок памяти обратно в пул свободных блоков.
    ///
    /// # Safety
    /// Указатель и размер должны в точности соответствовать ранее выделенной области.
    unsafe fn dealloc_raw(&mut self, ptr: *mut u8, size: usize, _align: usize) {
        let size = core::cmp::max(size, core::mem::size_of::<Hole>());
        let hole_ptr = ptr as *mut Hole;

        // Вставляем освобожденный блок в начало списка свободных блоков
        // SAFETY: Запись метаданных освобожденного блока.
        unsafe {
            (*hole_ptr).size = size;
            (*hole_ptr).next = self.head.next;
            self.head.next = Some(hole_ptr);
        }
    }
}

impl Default for LinkedHeapAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Глобальная обертка над кучевым аллокатором с синхронизацией через SpinMutex.
pub struct LockedHeap(SpinMutex<LinkedHeapAllocator>);

impl LockedHeap {
    /// Создает пустую обертку кучи.
    pub const fn empty() -> Self {
        Self(SpinMutex::new(LinkedHeapAllocator::new()))
    }

    /// Инициализирует глобальную кучу статическим буфером ядра.
    ///
    /// # Safety
    /// Вызывается ровно один раз при загрузке ядра до первой динамической аллокации.
    pub unsafe fn init(&self) {
        let mut allocator = self.0.lock();
        unsafe {
            let start = core::ptr::addr_of_mut!(HEAP_SPACE) as usize;
            allocator.init(start, HEAP_SIZE);
        }
    }
}

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut allocator = self.0.lock();
        // SAFETY: Безопасное выделение памяти внутри спинлока.
        unsafe {
            allocator.alloc_raw(layout.size(), layout.align()).unwrap_or(core::ptr::null_mut())
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let mut allocator = self.0.lock();
        // SAFETY: Возврат памяти внутри спинлока.
        unsafe {
            allocator.dealloc_raw(ptr, layout.size(), layout.align());
        }
    }
}

/// Зарегистрированный глобальный аллокатор ядра Hammam.
#[global_allocator]
pub static HEAP_ALLOCATOR: LockedHeap = LockedHeap::empty();
