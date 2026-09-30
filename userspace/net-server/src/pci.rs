/// PCI конфигурация для поиска virtio-net устройства
/// Vendor ID = 0x1AF4, Device ID = 0x1000 (virtio-net legacy)
///
/// TODO: заменить прямой I/O вызов на capability-запрос к ядру, когда syscall
/// для запроса I/O-порта capability будет реализован в Hammam.

#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub slot: u8,
    pub bar0: u16,
}

/// Найти virtio-net устройство на PCI шине (Vendor=0x1AF4, Device=0x1000).
pub fn find_virtio_net() -> Option<PciDevice> {
    for bus in 0u8..=255 {
        for slot in 0u8..32 {
            let vendor_device = pci_read32(bus, slot, 0, 0);
            if vendor_device == 0x1000_1AF4 {
                // Диагностика: печатаем сырые BAR0 и Command до любых правок.
                let bar0_raw = pci_read32(bus, slot, 0, 0x10);
                let cmd_raw = pci_read32(bus, slot, 0, 0x04);
                let cmd = (cmd_raw >> 16) & 0xFFFF;
                println!("[pci] found 1af4:1000 at bus={} slot={}", bus, slot);
                println!("[pci]   BAR0 raw = {:#010x}", bar0_raw);
                println!("[pci]   CMD  raw = {:#010x} status={:#06x} command={:#06x}",
                    cmd_raw, cmd_raw >> 16, cmd);
                println!("[pci]   I/O space (cmd bit0) = {}", cmd & 1);
                println!("[pci]   bus master  (cmd bit2) = {}", (cmd >> 2) & 1);
                let bar0 = bar0_raw & !0xF;
                return Some(PciDevice { bus, slot, bar0: bar0 as u16 });
            }
        }
    }
    None
}

fn pci_addr(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    0x8000_0000
        | (bus as u32) << 16
        | (slot as u32) << 11
        | (func as u32) << 8
        | (offset as u32 & 0xFC)
}

/// Прочитать 32-битное значение из PCI конфигурационного пространства.
/// Использует Port I/O через CONFIG_ADDRESS (0xCF8) и CONFIG_DATA (0xCFC).
///
/// TODO: на реальном Hammam потребуется capability на I/O порты.
/// Пока используем unsafe напрямую для первого теста.
fn pci_read32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let addr = pci_addr(bus, slot, func, offset);
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") 0xCF8u16, in("eax") addr, options(nostack));
        let val: u32;
        core::arch::asm!("in eax, dx", in("dx") 0xCFCu16, out("eax") val, options(nostack));
        val
    }
}
