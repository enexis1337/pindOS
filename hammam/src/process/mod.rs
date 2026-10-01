use crate::drivers::serial::SpinMutex;
use crate::cap::CapTable;
use crate::loader::elf::ElfLoader;
use crate::mm::{BuddyAllocator, PHYSICAL_ALLOCATOR, map_page, PageFlags};
use crate::sched::task::{Task, AddressSpace, Mutex, TaskState};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicI32, AtomicBool, Ordering};

/// Что умеет дескриптор.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FdKind {
    /// Консоль COM1: читаемый (stdin) и/или пишущий (stdout, stderr).
    Console,
}

/// Один открытый дескриптор.
#[derive(Clone, Copy, Debug)]
pub struct FdEntry {
    pub kind: FdKind,
    /// Можно ли читать: для консоли это fd 0.
    pub readable: bool,
    /// Можно ли писать: для консоли это fd 1 и fd 2.
    pub writable: bool,
}

/// Максимум дескрипторов на процесс.
pub const FD_TABLE_SIZE: usize = 16;

/// Таблица дескрипторов процесса.
///
/// По умолчанию процесс получает консоль на 0, 1 и 2, то есть поведение
/// совпадает с прежним жёстким `fd == 1`, но sys_write больше не смотрит
/// на номер напрямую.
#[derive(Clone)]
pub struct FdTable {
    entries: [Option<FdEntry>; FD_TABLE_SIZE],
}

impl FdTable {
    /// Новая таблица: 0 — читаемая консоль, 1 и 2 — пишущие.
    pub fn new() -> Self {
        let mut entries = [None; FD_TABLE_SIZE];
        entries[0] = Some(FdEntry { kind: FdKind::Console, readable: true,  writable: false });
        entries[1] = Some(FdEntry { kind: FdKind::Console, readable: false, writable: true  });
        entries[2] = Some(FdEntry { kind: FdKind::Console, readable: false, writable: true  });
        Self { entries }
    }

    pub fn get(&self, fd: u32) -> Option<FdEntry> {
        if fd as usize >= FD_TABLE_SIZE { return None; }
        self.entries[fd as usize]
    }

    /// Можно ли писать в этот дескриптор.
    pub fn is_writable(&self, fd: u32) -> bool {
        self.get(fd).map(|e| e.writable).unwrap_or(false)
    }

    /// Можно ли читать из этого дескриптора.
    pub fn is_readable(&self, fd: u32) -> bool {
        self.get(fd).map(|e| e.readable).unwrap_or(false)
    }

    /// Занять свободный дескриптор с запрошенным доступом.
    pub fn alloc(&mut self, kind: FdKind, readable: bool, writable: bool) -> Option<u32> {
        for i in 0..FD_TABLE_SIZE {
            if self.entries[i].is_none() {
                self.entries[i] = Some(FdEntry { kind, readable, writable });
                return Some(i as u32);
            }
        }
        None
    }

    /// Закрыть дескриптор. 0, 1 и 2 закрывать нельзя: без них процесс
    /// лишается консоли и не сможет сообщить об ошибке.
    pub fn close(&mut self, fd: u32) -> bool {
        if fd < 3 || fd as usize >= FD_TABLE_SIZE { return false; }
        self.entries[fd as usize].take().is_some()
    }
}

pub struct Process {
    pub pid:           u32,
    pub address_space: Arc<Mutex<AddressSpace>>,
    pub cap_table:     SpinMutex<CapTable>,
    pub main_task:     Arc<Task>,
    pub entry_point:   u64,
    pub user_stack_top: u64,
    pub exit_code:     AtomicI32,
    pub is_zombie:     AtomicBool,
    /// Дескрипторы процесса: 0 читает консоль, 1 и 2 пишут в неё.
    pub fd_table:      SpinMutex<FdTable>,
}

impl Process {
    pub fn is_zombie(&self) -> bool {
        self.is_zombie.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ProcessError {
    ElfError,
    AllocationError,
    StackAllocationError,
}

impl Process {
    /// Загружает ELF-парный сегмент и пользовательский стек в активное адресное
    /// пространство (вызывается при временно переключённом CR3 на корень процесса).
    fn load_user_into_root(
        elf_data: &[u8],
        allocator: &mut BuddyAllocator,
    ) -> Result<(u64, u64), ProcessError> {
        let mut loader = ElfLoader::new(elf_data, allocator);
        let entry_point = loader.load().map_err(|_| ProcessError::ElfError)?;

        const USER_STACK_PAGES: u64 = 16;
        let user_stack_vaddr: u64 = 0x08000000;
        let mut user_stack_bottom = user_stack_vaddr;
        let user_stack_vaddr_end = user_stack_vaddr + USER_STACK_PAGES * 0x1000;
        while user_stack_bottom < user_stack_vaddr_end {
            let frame = allocator
                .allocate(0)
                .map_err(|_| ProcessError::StackAllocationError)?;
            unsafe {
                map_page(
                    user_stack_bottom,
                    frame,
                    PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER_ACCESSIBLE,
                    allocator,
                )
                .map_err(|_| ProcessError::StackAllocationError)?;
            }
            user_stack_bottom += 0x1000;
        }
        let user_stack_top = user_stack_vaddr_end;

        Ok((entry_point, user_stack_top))
    }

    pub fn from_elf(pid: u32, elf_data: &[u8]) -> Result<Self, ProcessError> {
        let mut allocator = PHYSICAL_ALLOCATOR.lock();

        // Собственное адресное пространство: свежий PML4 с identity-скелетом ядра
        // (512 × 2 MiB). Пользовательские ELF/стек будут приватными для процесса.
        let as_root = crate::mm::copy_kernel_pml4(&mut allocator)
            .map_err(|_| ProcessError::AllocationError)?;

        // Временно активируем новое адресное пространство, чтобы ELF-загрузчик и
        // map_page работали в нём, а не в глобальных таблицах. Обе PML4 содержат
        // identity-карту ядра, поэтому код/стек ядра остаются доступны; внутри
        // sys_exec IF=0 (SFMASK), таймер не вмешается.
        let old_cr3 = crate::mm::active_pml4();
        unsafe { crate::mm::set_cr3(as_root); }

        let load_result = Self::load_user_into_root(elf_data, &mut allocator);

        // Возвращаемся в адресное пространство текущего процесса.
        unsafe { crate::mm::set_cr3(old_cr3); }

        let (entry_point, user_stack_top) = load_result?;

        let cap_table = SpinMutex::new(CapTable::new());

        let address_space = Arc::new(Mutex::new(AddressSpace { root_pml4: as_root }));

        let task = Arc::new(Task::new(
            crate::sched::task::TaskId(pid as u64),
            1,
            Arc::clone(&address_space),
        ));

        // Initialize task context for scheduler: when switched to, return to userspace via SYSRET
        unsafe {
            let task_ptr = Arc::as_ptr(&task) as *mut Task;
            let stack_top = (*task_ptr).kernel_stack.top;
            let stack_ptr = (stack_top - core::mem::size_of::<u64>()) as *mut u64;
            *stack_ptr = crate::arch::x86_64::syscall::return_to_userspace_trampoline as u64;
            (*task_ptr).context.rsp = stack_ptr as u64;
            (*task_ptr).context.cr3 = as_root.start_address;
            (*task_ptr).state = TaskState::Ready;
            (*task_ptr).user_entry = entry_point;
            (*task_ptr).user_stack = user_stack_top;
        }

        Ok(Process {
            pid,
            address_space,
            cap_table,
            fd_table: SpinMutex::new(FdTable::new()),
            main_task: task,
            entry_point,
            user_stack_top,
            exit_code: AtomicI32::new(0),
            is_zombie: AtomicBool::new(false),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn kernel_stack_top(&self) -> u64 {
        self.main_task.kernel_stack.top as u64
    }
}

pub static PROCESS_TABLE: SpinMutex<BTreeMap<u32, Arc<Process>>> =
    SpinMutex::new(BTreeMap::new());

pub static CURRENT_PROCESS: SpinMutex<Option<Arc<Process>>> = SpinMutex::new(None);

pub fn next_pid() -> u32 {
    static NEXT_PID: core::sync::atomic::AtomicU32 =
        core::sync::atomic::AtomicU32::new(2);
    NEXT_PID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}