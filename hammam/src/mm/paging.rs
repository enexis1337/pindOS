use crate::mm::physical::{BuddyAllocator, PhysFrame, AllocError};
use crate::drivers::serial::SpinMutex;

/// Стабильный «эталонный» корень ядра (boot-PML4), из которого клонируются
/// скелеты новых процессов. Захватывается при первом клонировании — в этот
/// момент active-таблицами ещё являются boot-таблицы (PID 1 создаётся до
/// любых переключений CR3), и они содержат только kernel-отображения.
static KERNEL_ROOT: SpinMutex<Option<PhysFrame>> = SpinMutex::new(None);

// Битовые флаги для записи таблицы страниц x86_64.
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PageFlags: u64 {
        /// Страница присутствует в памяти
        const PRESENT = 1 << 0;
        /// Разрешена запись на страницу
        const WRITABLE = 1 << 1;
        /// Доступна из Ring 3 (пространство пользователя)
        const USER_ACCESSIBLE = 1 << 2;
        /// Сквозная запись (Write-through caching)
        const WRITE_THROUGH = 1 << 3;
        /// Запретить кэширование страницы
        const CACHE_DISABLE = 1 << 4;
        /// Флаг обращения к странице процессором
        const ACCESSED = 1 << 5;
        /// Флаг модификации страницы (грязная страница)
        const DIRTY = 1 << 6;
        /// Большой фрейм (2 MiB в PD или 1 GiB в PDPT)
        const HUGE_PAGE = 1 << 7;
        /// Глобальная страница (не вымывается из TLB при перезаписи CR3)
        const GLOBAL = 1 << 8;
        /// Запрет выполнения кода на этой странице (No-Execute)
        const NO_EXECUTE = 1 << 63;
    }
}

/// Запись в таблице страниц (8 байт).
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl Default for PageTableEntry {
    fn default() -> Self {
        Self::new()
    }
}

impl PageTableEntry {
    /// Создает пустую запись.
    pub const fn new() -> Self {
        Self(0)
    }

    /// Проверяет, пуста ли запись.
    pub fn is_unused(&self) -> bool {
        self.0 == 0
    }

    /// Обнуляет запись.
    pub fn set_unused(&mut self) {
        self.0 = 0;
    }

    /// Возвращает флаги страницы.
    pub fn flags(&self) -> PageFlags {
        PageFlags::from_bits_truncate(self.0)
    }

    /// Возвращает соответствующий физический фрейм, если флаг PRESENT установлен.
    pub fn frame(&self) -> Option<PhysFrame> {
        if self.flags().contains(PageFlags::PRESENT) {
            // Выделяем адрес фрейма из бит 12..51 записей x86_64
            Some(PhysFrame::new(self.0 & 0x000F_FFFF_FFFF_F000))
        } else {
            None
        }
    }

    /// Связывает запись с физическим фреймом и устанавливает флаги.
    pub fn set_frame(&mut self, frame: PhysFrame, flags: PageFlags) {
        let addr = frame.start_address & 0x000F_FFFF_FFFF_F000;
        self.0 = addr | flags.bits();
    }
}

/// Таблица страниц x86_64, содержащая ровно 512 записей по 8 байт (ровно 4096 байт).
#[repr(align(4096))]
pub struct PageTable {
    pub(crate) entries: [PageTableEntry; 512],
}

impl Default for PageTable {
    fn default() -> Self {
        Self::new()
    }
}

impl PageTable {
    /// Создает чистую таблицу страниц.
    pub const fn new() -> Self {
        Self {
            entries: [PageTableEntry::new(); 512],
        }
    }

    /// Обнуляет все записи таблицы страниц.
    pub fn zero(&mut self) {
        for entry in self.entries.iter_mut() {
            entry.set_unused();
        }
    }
}

/// Ошибки при отображении виртуальных страниц.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// Не удалось выделить физическую страницу для промежуточной таблицы.
    FrameAllocationFailed,
    /// Виртуальный адрес уже отображен на физическую память.
    AlreadyMapped,
}

/// Ошибки при снятии отображения виртуальных страниц.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmapError {
    /// Виртуальный адрес не отображен.
    NotMapped,
}

/// Вспомогательные функции для расчета индексов в 4-уровневой иерархии страниц.
#[inline]
fn pml4_index(virt: u64) -> usize {
    ((virt >> 39) & 0x1FF) as usize
}

#[inline]
fn pdpt_index(virt: u64) -> usize {
    ((virt >> 30) & 0x1FF) as usize
}

#[inline]
fn pd_index(virt: u64) -> usize {
    ((virt >> 21) & 0x1FF) as usize
}

#[inline]
fn pt_index(virt: u64) -> usize {
    ((virt >> 12) & 0x1FF) as usize
}

/// Возвращает адрес физического фрейма активной PML4 таблицы страниц из регистра CR3.
pub fn active_pml4() -> PhysFrame {
    let cr3: u64;
    // SAFETY: Чтение системного регистра CR3 допустимо только в Ring 0.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    }
    PhysFrame::new(cr3 & 0x000F_FFFF_FFFF_F000)
}

/// Преобразует физический фрейм в мутабельную ссылку на структуру PageTable.
///
/// # Safety
/// Физический адрес фрейма должен быть отображен в виртуальное адресное пространство.
/// В условиях начального identity mapping физический адрес равен виртуальному адресу.
#[inline]
unsafe fn get_table<'a>(frame: PhysFrame) -> &'a mut PageTable {
    // SAFETY: Вызывающий гарантирует, что фрейм отображен и указывает на валидную таблицу страниц.
    unsafe { &mut *(frame.start_address as *mut PageTable) }
}

/// Отображает виртуальную страницу на физический фрейм в активном адресном пространстве.
/// Создает недостающие промежуточные таблицы страниц при необходимости.
///
/// # Safety
/// Физический фрейм должен быть валидным и не занятым.
pub unsafe fn map_page(
    virt: u64,
    phys: PhysFrame,
    flags: PageFlags,
    allocator: &mut BuddyAllocator,
) -> Result<(), MapError> {
    let pml4_addr = active_pml4();
    // SAFETY: PML4 гарантированно присутствует в памяти и адрес взят из CR3.
    let pml4 = unsafe { get_table(pml4_addr) };

    // PML4 -> PDPT
    let pml4_idx = pml4_index(virt);
    let pdpt_frame = if pml4.entries[pml4_idx].flags().contains(PageFlags::PRESENT) {
        pml4.entries[pml4_idx].frame().unwrap()
    } else {
        let frame = allocator.allocate(0).map_err(|_| MapError::FrameAllocationFailed)?;
        // SAFETY: Нововыделенный фрейм чистится и размечается под таблицу страниц.
        unsafe {
            let table = get_table(frame);
            table.zero();
        }
        pml4.entries[pml4_idx].set_frame(frame, PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER_ACCESSIBLE);
        frame
    };

    // PDPT -> PD
    // SAFETY: Ссылка на валидную PDPT таблицу.
    let pdpt = unsafe { get_table(pdpt_frame) };
    let pdpt_idx = pdpt_index(virt);
    let pd_frame = if pdpt.entries[pdpt_idx].flags().contains(PageFlags::PRESENT) {
        pdpt.entries[pdpt_idx].frame().unwrap()
    } else {
        let frame = allocator.allocate(0).map_err(|_| MapError::FrameAllocationFailed)?;
        // SAFETY: Очистка фрейма под таблицу.
        unsafe {
            let table = get_table(frame);
            table.zero();
        }
        pdpt.entries[pdpt_idx].set_frame(frame, PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER_ACCESSIBLE);
        frame
    };

    // PD -> PT
    // SAFETY: Ссылка на валидную PD таблицу.
    let pd = unsafe { get_table(pd_frame) };
    let pd_idx = pd_index(virt);

    // Если PD entry — огромная страница (2 MiB), разбиваем её на 4 KiB страницы
    if pd.entries[pd_idx].flags().contains(PageFlags::PRESENT) && pd.entries[pd_idx].flags().contains(PageFlags::HUGE_PAGE) {
        let huge_phys = pd.entries[pd_idx].frame().unwrap().start_address;
        let pt_frame = allocator.allocate(0).map_err(|_| MapError::FrameAllocationFailed)?;
        // SAFETY: Заполняем новую таблицу страниц.
        let pt = unsafe { get_table(pt_frame) };
        let huge_flags = pd.entries[pd_idx].flags() & !PageFlags::HUGE_PAGE;
        for i in 0..512 {
            let phys = PhysFrame::new(huge_phys + (i * 0x1000) as u64);
            pt.entries[i].set_frame(phys, huge_flags);
        }
        let pd_flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER_ACCESSIBLE;
        pd.entries[pd_idx].set_frame(pt_frame, pd_flags);
    }

    let pt_frame = if pd.entries[pd_idx].flags().contains(PageFlags::PRESENT) {
        pd.entries[pd_idx].frame().unwrap()
    } else {
        let frame = allocator.allocate(0).map_err(|_| MapError::FrameAllocationFailed)?;
        // SAFETY: Очистка фрейма под таблицу.
        unsafe {
            let table = get_table(frame);
            table.zero();
        }
        pd.entries[pd_idx].set_frame(frame, PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER_ACCESSIBLE);
        frame
    };

    // PT -> Физический фрейм данных
    // SAFETY: Ссылка на валидную PT таблицу.
    let pt = unsafe { get_table(pt_frame) };
    let pt_idx = pt_index(virt);
    // If already mapped (e.g. after huge-page split), overwrite.
    pt.entries[pt_idx].set_frame(phys, flags | PageFlags::PRESENT);

    // Flush TLB entry so the CPU sees the new mapping (huge→4KiB split or remap)
    unsafe { core::arch::asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags)); }

    // Debug: verify the mapping
    if virt == 0x400000 {
        unsafe { crate::drivers::serial::SERIAL.get().write_byte(b'M'); }
    }

    Ok(())
}

/// Снимает отображение виртуальной страницы и инвалидирует её в кэше TLB (invlpg).
///
/// # Safety
/// Вызывается в Ring 0. Изменение таблиц страниц должно быть согласовано с остальной архитектурой.
pub unsafe fn unmap_page(virt: u64) -> Result<PhysFrame, UnmapError> {
    let pml4_addr = active_pml4();
    // SAFETY: PML4 гарантированно присутствует в памяти.
    let pml4 = unsafe { get_table(pml4_addr) };

    // PML4 -> PDPT
    let pml4_idx = pml4_index(virt);
    if !pml4.entries[pml4_idx].flags().contains(PageFlags::PRESENT) {
        return Err(UnmapError::NotMapped);
    }
    let pdpt_frame = pml4.entries[pml4_idx].frame().unwrap();

    // PDPT -> PD
    // SAFETY: Ссылка на валидную PDPT.
    let pdpt = unsafe { get_table(pdpt_frame) };
    let pdpt_idx = pdpt_index(virt);
    if !pdpt.entries[pdpt_idx].flags().contains(PageFlags::PRESENT) {
        return Err(UnmapError::NotMapped);
    }
    let pd_frame = pdpt.entries[pdpt_idx].frame().unwrap();

    // PD -> PT
    // SAFETY: Ссылка на валидную PD.
    let pd = unsafe { get_table(pd_frame) };
    let pd_idx = pd_index(virt);
    if !pd.entries[pd_idx].flags().contains(PageFlags::PRESENT) {
        return Err(UnmapError::NotMapped);
    }
    let pt_frame = pd.entries[pd_idx].frame().unwrap();

    // PT -> Физический адрес фрейма данных
    // SAFETY: Ссылка на валидную PT.
    let pt = unsafe { get_table(pt_frame) };
    let pt_idx = pt_index(virt);
    if !pt.entries[pt_idx].flags().contains(PageFlags::PRESENT) {
        return Err(UnmapError::NotMapped);
    }
    let frame = pt.entries[pt_idx].frame().unwrap();
    pt.entries[pt_idx].set_unused();

    // Инвалидация записи в TLB процессора
    // SAFETY: Инструкция invlpg сбрасывает виртуальный адрес из кэша трансляции процессора.
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags));
    }

    Ok(frame)
}

/// Транслирует виртуальный адрес во флаги записи таблицы страниц.
/// Возвращает флаги конечной записи (PT) или флаги huge-page (PD/PDPT).
pub fn translate_flags(virt: u64) -> Option<PageFlags> {
    let pml4_addr = active_pml4();
    let pml4 = unsafe { get_table(pml4_addr) };

    let pml4_idx = pml4_index(virt);
    if !pml4.entries[pml4_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    let pdpt_frame = pml4.entries[pml4_idx].frame()?;

    let pdpt = unsafe { get_table(pdpt_frame) };
    let pdpt_idx = pdpt_index(virt);
    if !pdpt.entries[pdpt_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    // 1 GiB huge page in PDPT
    if pdpt.entries[pdpt_idx].flags().contains(PageFlags::HUGE_PAGE) {
        return Some(pdpt.entries[pdpt_idx].flags());
    }
    let pd_frame = pdpt.entries[pdpt_idx].frame()?;

    let pd = unsafe { get_table(pd_frame) };
    let pd_idx = pd_index(virt);
    if !pd.entries[pd_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    // 2 MiB huge page in PD
    if pd.entries[pd_idx].flags().contains(PageFlags::HUGE_PAGE) {
        return Some(pd.entries[pd_idx].flags());
    }
    let pt_frame = pd.entries[pd_idx].frame()?;

    let pt = unsafe { get_table(pt_frame) };
    let pt_idx = pt_index(virt);
    if !pt.entries[pt_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    Some(pt.entries[pt_idx].flags())
}

/// Загружает выбранный физический фрейм PML4 в регистр CR3 (переключает адресное пространство).
///
/// # Safety
/// Должно вызываться из Ring 0. Фрейм должен указывать на валидную PML4,
/// содержащую отображение хотя бы текущих инструкций и стека ядра.
pub unsafe fn set_cr3(frame: PhysFrame) {
    unsafe {
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) frame.start_address,
            options(nostack, preserves_flags)
        );
    }
}

/// Собирает свежую «скелетную» PML4 для нового процесса, клонируя kernel-карту.
///
/// Возвращаемая PML4 содержит:
/// - приватный PD с копией 512 × 2 MiB huge-page записей identity-карты 1 GiB
///   (в нём каждый процесс приватно разбивает huge-страницы под ELF/стек);
/// - остальные настоящие kernel-записи PDPT (APIC MMIO и т.п.) разделяются с
///   эталонным корнем ядра — они никогда не модифицируются процессами.
///
/// Пользовательские отображения (ELF, стек) каждый процесс создаёт в
/// собственных (не разделяемых) PD/PT.
pub fn copy_kernel_pml4(allocator: &mut BuddyAllocator) -> Result<PhysFrame, AllocError> {
    // Эталонный корень берём из кэша (захватывается один раз при первом
    // клонировании — это pristine boot-таблицы, см. KERNEL_ROOT).
    let root = {
        let mut cache = KERNEL_ROOT.lock();
        if cache.is_none() {
            *cache = Some(active_pml4());
        }
        cache.unwrap()
    };

    // SAFETY: эталонный корень присутствует (identity-карта ядра).
    let boot_pml4 = unsafe { get_table(root) };
    let boot_pdpt_frame = boot_pml4.entries[0]
        .frame()
        .ok_or(AllocError::OutOfMemory)?;

    // SAFETY: фрейм PDPT из валидной иерархии эталонных таблиц.
    let boot_pdpt = unsafe { get_table(boot_pdpt_frame) };
    let boot_pd_frame = boot_pdpt.entries[0]
        .frame()
        .ok_or(AllocError::OutOfMemory)?;

    // Выделяем свежие фреймы: PML4, PDPT, PD.
    let new_pml4_frame = allocator.allocate(0)?;
    let new_pdpt_frame = allocator.allocate(0)?;
    let new_pd_frame = allocator.allocate(0)?;

    // Приватный PD: копия 512 identity huge-page записей.
    // SAFETY: оба фрейма отображены в identity-карте (физ. == вирт. адрес);
    // содержимое PD ещё не используется никем, кроме копирующего кода.
    let src_pd = unsafe { get_table(boot_pd_frame) };
    let dst_pd = unsafe { get_table(new_pd_frame) };
    unsafe {
        core::ptr::copy_nonoverlapping(
            src_pd.entries.as_ptr(),
            dst_pd.entries.as_mut_ptr(),
            512,
        );
    }

    // Приватный PDPT: identity-PD приватный, остальные kernel-записи разделяем.
    // SAFETY: свежевыделенный фрейм PDPT обнуляем и размечаем.
    let new_pdpt = unsafe { get_table(new_pdpt_frame) };
    new_pdpt.zero();
    let pdpt_flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER_ACCESSIBLE;
    new_pdpt.entries[0].set_frame(new_pd_frame, pdpt_flags);
    for i in 1..512 {
        if boot_pdpt.entries[i].flags().contains(PageFlags::PRESENT) {
            new_pdpt.entries[i] = boot_pdpt.entries[i];
        }
    }

    // Приватный PML4.
    // SAFETY: свежевыделенный фрейм PML4 обнуляем и привязываем PDPT.
    let new_pml4 = unsafe { get_table(new_pml4_frame) };
    new_pml4.zero();
    new_pml4.entries[0].set_frame(new_pdpt_frame, pdpt_flags);

    Ok(new_pml4_frame)
}

/// Транслирует виртуальный адрес в физический адрес в произвольном (не обязательно
/// активном) адресном пространстве. Поддерживает 4 KiB, 2 MiB и 1 GiB страницы.
pub fn translate_in(root: PhysFrame, virt: u64) -> Option<u64> {
    // SAFETY: таблицы адресного пространства отображены в identity-карте ядра.
    let pml4 = unsafe { get_table(root) };

    let pml4_idx = pml4_index(virt);
    if !pml4.entries[pml4_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    let pdpt_frame = pml4.entries[pml4_idx].frame()?;

    // SAFETY: фрейм PDPT из валидной иерархии.
    let pdpt = unsafe { get_table(pdpt_frame) };
    let pdpt_idx = pdpt_index(virt);
    if !pdpt.entries[pdpt_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    // 1 GiB huge page в PDPT
    if pdpt.entries[pdpt_idx].flags().contains(PageFlags::HUGE_PAGE) {
        return Some(pdpt.entries[pdpt_idx].frame()?.start_address + (virt & 0x3FFF_FFFF));
    }
    let pd_frame = pdpt.entries[pdpt_idx].frame()?;

    // SAFETY: фрейм PD из валидной иерархии.
    let pd = unsafe { get_table(pd_frame) };
    let pd_idx = pd_index(virt);
    if !pd.entries[pd_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    // 2 MiB huge page в PD
    if pd.entries[pd_idx].flags().contains(PageFlags::HUGE_PAGE) {
        return Some(pd.entries[pd_idx].frame()?.start_address + (virt & 0x001F_FFFF));
    }
    let pt_frame = pd.entries[pd_idx].frame()?;

    // SAFETY: фрейм PT из валидной иерархии.
    let pt = unsafe { get_table(pt_frame) };
    let pt_idx = pt_index(virt);
    if !pt.entries[pt_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    let frame = pt.entries[pt_idx].frame()?;
    Some(frame.start_address + (virt & 0xFFF))
}

/// Транслирует виртуальный адрес во флаги PT-записи в произвольном (не обязательно
/// активном) адресном пространстве. Поддерживает 4 KiB, 2 MiB и 1 GiB страницы.
pub fn translate_flags_in(root: PhysFrame, virt: u64) -> Option<PageFlags> {
    // SAFETY: таблицы адресного пространства отображены в identity-карте ядра.
    let pml4 = unsafe { get_table(root) };

    let pml4_idx = pml4_index(virt);
    if !pml4.entries[pml4_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    let pdpt_frame = pml4.entries[pml4_idx].frame()?;

    // SAFETY: фрейм PDPT из валидной иерархии.
    let pdpt = unsafe { get_table(pdpt_frame) };
    let pdpt_idx = pdpt_index(virt);
    if !pdpt.entries[pdpt_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    if pdpt.entries[pdpt_idx].flags().contains(PageFlags::HUGE_PAGE) {
        return Some(pdpt.entries[pdpt_idx].flags());
    }
    let pd_frame = pdpt.entries[pdpt_idx].frame()?;

    // SAFETY: фрейм PD из валидной иерархии.
    let pd = unsafe { get_table(pd_frame) };
    let pd_idx = pd_index(virt);
    if !pd.entries[pd_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    if pd.entries[pd_idx].flags().contains(PageFlags::HUGE_PAGE) {
        return Some(pd.entries[pd_idx].flags());
    }
    let pt_frame = pd.entries[pd_idx].frame()?;

    // SAFETY: фрейм PT из валидной иерархии.
    let pt = unsafe { get_table(pt_frame) };
    let pt_idx = pt_index(virt);
    if !pt.entries[pt_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    Some(pt.entries[pt_idx].flags())
}

/// Транслирует виртуальный адрес в физический адрес на основе активных таблиц страниц.
/// Поддерживает стандартные страницы 4 KiB и большие страницы 2 MiB.
pub fn translate(virt: u64) -> Option<u64> {
    let pml4_addr = active_pml4();
    // SAFETY: PML4 гарантированно присутствует в памяти при работе ядра.
    let pml4 = unsafe { get_table(pml4_addr) };

    // PML4 -> PDPT
    let pml4_idx = pml4_index(virt);
    if !pml4.entries[pml4_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    let pdpt_frame = pml4.entries[pml4_idx].frame()?;

    // PDPT -> PD
    // SAFETY: Валидный фрейм таблицы.
    let pdpt = unsafe { get_table(pdpt_frame) };
    let pdpt_idx = pdpt_index(virt);
    if !pdpt.entries[pdpt_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    let pd_frame = pdpt.entries[pdpt_idx].frame()?;

    // PD -> PT или Huge Page
    // SAFETY: Валидный фрейм таблицы.
    let pd = unsafe { get_table(pd_frame) };
    let pd_idx = pd_index(virt);
    if !pd.entries[pd_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }

    // Обработка 2 MiB Huge Page (бит HUGE_PAGE в каталоге страниц PD)
    if pd.entries[pd_idx].flags().contains(PageFlags::HUGE_PAGE) {
        let phys_addr = pd.entries[pd_idx].frame()?.start_address;
        let page_offset = virt & 0x001F_FFFF; // Смещение внутри 2 MiB
        return Some(phys_addr + page_offset);
    }

    let pt_frame = pd.entries[pd_idx].frame()?;

    // PT -> Фрейм данных
    // SAFETY: Валидный фрейм таблицы.
    let pt = unsafe { get_table(pt_frame) };
    let pt_idx = pt_index(virt);
    if !pt.entries[pt_idx].flags().contains(PageFlags::PRESENT) {
        return None;
    }
    let frame = pt.entries[pt_idx].frame()?;
    let page_offset = virt & 0xFFF; // Смещение внутри 4 KiB
    Some(frame.start_address + page_offset)
}
