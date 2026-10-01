use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use core::cell::UnsafeCell;

/// Простой Spinlock Mutex для синхронизации доступа к аппаратуре в no_std окружении.
pub struct SpinMutex<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinMutex<T> {}
unsafe impl<T: Send> Send for SpinMutex<T> {}

impl<T> SpinMutex<T> {
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> SpinMutexGuard<'_, T> {
        while self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            // Ждем освобождения блокировки (spin loop)
            core::hint::spin_loop();
        }
        SpinMutexGuard { mutex: self }
    }
}

/// Guard для автоматического освобождения блокировки при выходе из области видимости.
pub struct SpinMutexGuard<'a, T> {
    mutex: &'a SpinMutex<T>,
}

impl<'a, T> core::ops::Deref for SpinMutexGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // SAFETY: Мы захватили блокировку locked = true, поэтому доступ к данным эксклюзивен.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> core::ops::DerefMut for SpinMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: Мы захватили блокировку locked = true, поэтому доступ к данным эксклюзивен.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for SpinMutexGuard<'a, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
    }
}

/// Базовый адрес первого последовательного порта COM1
pub const COM1_BASE: u16 = 0x3F8;

/// Драйвер UART 16550 последовательного порта для вывода логов ядра.
pub struct Serial(u16);

impl Serial {
    /// Создает новый экземпляр драйвера последовательного порта с указанной базой портов.
    pub const fn new(port: u16) -> Self {
        Self(port)
    }

    /// Инициализирует последовательный порт для работы на реальном ПК.
    ///
    /// # Safety
    /// Функция осуществляет запись в порты ввода-вывода (Port I/O). Должна вызываться один раз при старте ядра.
    pub unsafe fn init(&self) {
        // SAFETY: Настройка регистров UART в соответствии со спецификацией 16550.
        unsafe {
            self.outb(1, 0x00); // Отключение всех прерываний порта
            self.outb(3, 0x80); // Включение DLAB (Divisor Latch Access Bit) для установки скорости
            self.outb(0, 0x03); // Делитель частоты: 3 (115200 / 3 = 38400 бод) - младший байт
            self.outb(1, 0x00); // Старший байт делителя
            self.outb(3, 0x03); // Режим работы: 8 бит данных, без четности, 1 стоп-бит (DLAB отключается)
            self.outb(2, 0xC7); // Включение FIFO, очистка буферов передачи/приема, триггер на 14 байт
            self.outb(4, 0x0B); // Активация DTR, RTS и Out2 (необходимо для работы аппаратного управления потоком)
        }
    }

    /// Есть ли принятый байт: LSR бит 0 (Data Ready).
    pub fn is_receive_ready(&self) -> bool {
        // SAFETY: чтение LSR.
        unsafe { (self.inb(5) & 0x01) != 0 }
    }

    /// Прочитать один принятый байт из RBR (смещение +0).
    pub fn read_byte(&self) -> u8 {
        // SAFETY: чтение RBR, вызывается только при is_receive_ready().
        unsafe { self.inb(0) }
    }

    /// Включить прерывание приёма: IER (смещение +1) бит 0.
    ///
    /// Прерывание на приём через IO-APIC мы пока не маршрутизируем, поэтому
    /// байты забирает опрос из таймерного тика. IER нужен, чтобы UART вообще
    /// сигналил о готовности данных.
    pub unsafe fn enable_rx_interrupt(&self) {
        // SAFETY: запись IER.
        unsafe { self.outb(1, 0x01) }
    }

    /// Проверяет, пуст ли передающий буфер UART.
    fn is_transmit_empty(&self) -> bool {
        // Line Status Register (LSR) находится на смещении +5. Bit 5 = Transmit Holding Register Empty.
        // SAFETY: Чтение статуса порта.
        unsafe { (self.inb(5) & 0x20) != 0 }
    }

    /// Записывает один байт данных в последовательный порт.
    pub fn write_byte(&self, byte: u8) {
        // Ожидаем готовности передатчика
        while !self.is_transmit_empty() {
            core::hint::spin_loop();
        }
        // Записываем байт в Transmitter Holding Register на смещении +0
        // SAFETY: Запись байта в порт вывода.
        unsafe {
            self.outb(0, byte);
        }
    }

    /// Выводит байт в виде двух hex-символов (00–FF)
    pub fn write_hex(&self, value: u8) {
        let hex = b"0123456789ABCDEF";
        self.write_byte(hex[(value >> 4) as usize]);
        self.write_byte(hex[(value & 0x0F) as usize]);
    }

    /// Вспомогательная функция для записи байта в порт (outb)
    #[inline]
    unsafe fn outb(&self, offset: u16, value: u8) {
        // SAFETY: Прямая запись в порт ввода-вывода.
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") self.0 + offset,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            );
        }
    }

    /// Вспомогательная функция для чтения байта из порта (inb)
    #[inline]
    unsafe fn inb(&self, offset: u16) -> u8 {
        let value: u8;
        // SAFETY: Прямое чтение из порта ввода-вывода.
        unsafe {
            core::arch::asm!(
                "in al, dx",
                out("al") value,
                in("dx") self.0 + offset,
                options(nomem, nostack, preserves_flags)
            );
        }
        value
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // На реальных ПК часто требуется переводить \n в \r\n для корректного отображения в терминале
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

/// Wrapper вокруг UnsafeCell для реализации Sync (ядро однопоточное).
pub struct SerialPort(UnsafeCell<Serial>);

unsafe impl Sync for SerialPort {}

impl SerialPort {
    pub unsafe fn get(&self) -> &mut Serial {
        // SAFETY: Вызывающий должен гарантировать отсутствие гонок.
        unsafe { &mut *self.0.get() }
    }
}

/// Глобальный экземпляр последовательного порта (без блокировки — ядро однопоточное).
pub static SERIAL: SerialPort = SerialPort(UnsafeCell::new(Serial::new(COM1_BASE)));

/// Ёмкость кольцевого буфера приёма COM1.
pub const RX_RING_SIZE: usize = 256;

/// Кольцевой буфер приёма COM1.
///
/// Заполняется из таймерного тика, поэтому работает без аллокаций и без
/// прерываний: это важно, пока приём UART не подключён к IO-APIC.
/// Индексы монотонные и оборачиваются по маске, буфер — степени двойки.
pub struct RxRing {
    /// Буфер в UnsafeCell: пишет и читает контекст прерывания/тика.
    buf:   core::cell::UnsafeCell<[u8; RX_RING_SIZE]>,
    head:  core::sync::atomic::AtomicUsize,
    tail:  core::sync::atomic::AtomicUsize,
}

impl RxRing {
    pub const fn new() -> Self {
        Self {
            buf: core::cell::UnsafeCell::new([0; RX_RING_SIZE]),
            head: core::sync::atomic::AtomicUsize::new(0),
            tail: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Положить байт. Переполнение молча теряет самый старый байт: консоль
    /// не должна ронять систему из-за темпа набора.
    pub fn push(&self, b: u8) {
        let head = self.head.load(Ordering::Relaxed);
        let next = (head + 1) & (RX_RING_SIZE - 1);
        if next == self.tail.load(Ordering::Relaxed) {
            let t = self.tail.load(Ordering::Relaxed);
            self.tail.store((t + 1) & (RX_RING_SIZE - 1), Ordering::Relaxed);
        }
        // SAFETY: единственный писатель — контекст прерывания/тика.
        unsafe { (*self.buf.get())[head] = b; }
        self.head.store((head + 1) & (RX_RING_SIZE - 1), Ordering::Release);
    }

    /// Забрать один байт, если есть.
    pub fn pop(&self) -> Option<u8> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: единственный читатель — задача в системном вызове.
        let b = unsafe { (*self.buf.get())[tail] };
        self.tail.store((tail + 1) & (RX_RING_SIZE - 1), Ordering::Release);
        Some(b)
    }

    /// Есть ли что читать.
    pub fn has_data(&self) -> bool {
        self.tail.load(Ordering::Acquire) != self.head.load(Ordering::Acquire)
    }
}

unsafe impl Sync for RxRing {}

/// Отдельный контекстный буфер для эха: пока sys_read не готов, кольцо
/// возвращается обратно в порт, чтобы было видно, что приём вообще идёт.
pub static RX_ECHO: SpinMutex<RxRing> = SpinMutex::new(RxRing::new());

/// Переложить принятое из кольца в эхо-буфер.
pub fn push_rx_echo(b: u8) {
    RX_ECHO.lock().push(b);
}

/// Забрать накопленное из эхо-буфера, если есть.
pub fn pop_rx_echo() -> Option<u8> {
    RX_ECHO.lock().pop()
}

pub fn has_rx_echo() -> bool {
    RX_ECHO.lock().has_data()
}

pub static RX_RING: RxRing = RxRing::new();

/// Забрать всё накопленное с COM1 в кольцевой буфер ядра.
///
/// Вызывается из таймерного тика: прерывание приёма UART включено, но не
/// маршрутизируется через IO-APIC, поэтому опрос LSR — единственный путь.
pub unsafe fn poll_serial_rx() -> usize {
    let mut n = 0;
    // SAFETY: опрос порта из контекста прерывания допустим.
    unsafe {
        while SERIAL.get().is_receive_ready() {
            let b = SERIAL.get().read_byte();
            RX_RING.push(b);
            push_rx_echo(b);
            n += 1;
            if n >= RX_RING_SIZE { break; }
        }
    }
    n
}

/// Макрос для вывода форматированной строки в COM-порт ядра Hammam.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        // SAFETY: Ядро однопоточное на этапе загрузки — без блокировки безопасен.
        let serial = unsafe { $crate::drivers::serial::SERIAL.get() };
        <$crate::drivers::serial::Serial as core::fmt::Write>::write_fmt(serial, format_args!($($arg)*)).ok();
    };
}

/// Макрос для вывода строки с переносом строки в COM-порт ядра Hammam.
#[macro_export]
macro_rules! kprintln {
    () => ($crate::kprint!("\n"));
    ($($arg:tt)*) => ($crate::kprint!("{}\n", format_args!($($arg)*)));
}
