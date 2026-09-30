// userspace/net-server/src/device.rs

use smoltcp::phy::{Device, DeviceCapabilities, RxToken, TxToken, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::Ipv4Address;
use crate::virtio::Virtqueue;
use alloc::vec::Vec;

pub struct VirtioNetDevice {
    pub rx_queue: Virtqueue,
    pub tx_queue: Virtqueue,
    pub rx_buf:   [u8; 1514],
    /// Счётчики для диагностики ARP (наблюдаем в сыром кадре).
    pub arp_req_tx: u64,
    pub arp_reply_rx: u64,
    pub arp_request_rx: u64,
    pub frames_rx: u64,
    pub frames_tx: u64,
}

const ETHERTYPE_ARP: u16 = 0x0806;
const ARP_OP_REQUEST: u16 = 1;
const ARP_OP_REPLY: u16 = 2;

/// Разбирает Ethernet+ARP заголовок и печатает, что произошло.
///
/// 12 байт MAC назначения, 12 байт MAC источника, 2 байта ethertype, затем
/// ARP: htype(2) ptype(2) hlen(1) plen(1) oper(2) sha(6) spa(4) tha(6) tpa(4).
/// Целевой IP (tpa при request, spa при reply) лежит по смещению 0x26 от начала
/// кадра, назначение запроса — по 0x30.
fn trace_arp(frame: &[u8], tx: bool) {
    if frame.len() < 42 {
        return;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != ETHERTYPE_ARP {
        return;
    }
    let oper = u16::from_be_bytes([frame[20], frame[21]]);
    let spa = Ipv4Address::new(frame[28], frame[29], frame[30], frame[31]);
    let tpa = Ipv4Address::new(frame[38], frame[39], frame[40], frame[41]);
    let dir = if tx { "TX" } else { "RX" };
    match oper {
        ARP_OP_REQUEST => println!("[arp] {} request: who has {}? tell {}", dir, tpa, spa),
        ARP_OP_REPLY   => println!("[arp] {} reply: {} is at {:02x?}:{:02x?}:{:02x?}:{:02x?}:{:02x?}:{:02x?}",
            dir, spa, frame[22], frame[23], frame[24], frame[25], frame[26], frame[27]),
        _ => println!("[arp] {} unknown oper={}", dir, oper),
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxToken where Self: 'a;
    type TxToken<'a> = VirtioTxToken<'a> where Self: 'a;

    fn receive(&mut self, _: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let len = unsafe { self.rx_queue.recv(&mut self.rx_buf)? };
        self.frames_rx += 1;
        if len >= 42 {
            let oper = u16::from_be_bytes([self.rx_buf[20], self.rx_buf[21]]);
            let ethertype = u16::from_be_bytes([self.rx_buf[12], self.rx_buf[13]]);
            if ethertype == ETHERTYPE_ARP {
                match oper {
                    ARP_OP_REPLY   => self.arp_reply_rx += 1,
                    ARP_OP_REQUEST => self.arp_request_rx += 1,
                    _ => {}
                }
                trace_arp(&self.rx_buf[..len], false);
            }
        }
        let data = self.rx_buf[..len].to_vec();
        // Дисъюнктные заимствования полей: tx_queue и счётчики — разные поля.
        let tx_queue = &mut self.tx_queue;
        let frames_tx = &mut self.frames_tx;
        Some((
            VirtioRxToken { buf: data },
            VirtioTxToken { queue: tx_queue, frames_tx },
        ))
    }

    fn transmit(&mut self, _: Instant) -> Option<Self::TxToken<'_>> {
        let tx_queue = &mut self.tx_queue;
        let frames_tx = &mut self.frames_tx;
        Some(VirtioTxToken { queue: tx_queue, frames_tx })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

pub struct VirtioRxToken { buf: Vec<u8> }
pub struct VirtioTxToken<'a> {
    queue: &'a mut Virtqueue,
    frames_tx: &'a mut u64,
}

impl RxToken for VirtioRxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(mut self, f: F) -> R {
        f(&mut self.buf)
    }
}

impl<'a> TxToken for VirtioTxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = alloc::vec![0u8; len];
        let result = f(&mut buf);
        *self.frames_tx += 1;
        if len >= 42 {
            let ethertype = u16::from_be_bytes([buf[12], buf[13]]);
            if ethertype == ETHERTYPE_ARP {
                trace_arp(&buf[..len], true);
            }
        }
        unsafe { self.queue.send(&buf) };
        result
    }
}
