//! Host tests of the production ring. No UART/IRQ functions are executed.
//! rustc --edition=2021 --test tools/test_console_ring.rs -o /tmp/pindos-ring-test
#![allow(dead_code)]

mod sched {
    pub const WAIT_CONSOLE: u64 = 1;
    pub fn wake(_: u64) {}
}

#[path = "../hammam/src/drivers/serial.rs"]
mod serial;

use serial::{RxRing, RX_RING_SIZE};

#[test]
fn empty_full_and_drop_newest() {
    let ring = RxRing::new();
    assert!(!ring.has_data());
    assert_eq!(ring.pop(), None);
    for i in 0..RX_RING_SIZE - 1 {
        assert!(ring.push(i as u8));
    }
    assert!(!ring.push(0xff));
    for i in 0..RX_RING_SIZE - 1 {
        assert_eq!(ring.pop(), Some(i as u8));
    }
    assert!(!ring.has_data());
    assert_eq!(ring.pop(), None);
}

#[test]
fn wrap_and_reuse_slots() {
    let ring = RxRing::new();
    for round in 0..100 {
        for i in 0..200 {
            assert!(ring.push((round + i) as u8));
        }
        for i in 0..100 {
            assert_eq!(ring.pop(), Some((round + i) as u8));
        }
        for i in 200..300 {
            assert!(ring.push((round + i) as u8));
        }
        for i in 100..300 {
            assert_eq!(ring.pop(), Some((round + i) as u8));
        }
        assert_eq!(ring.pop(), None);
    }
}

#[test]
fn concurrent_single_producer_single_consumer() {
    use std::sync::Arc;
    let ring = Arc::new(RxRing::new());
    let producer_ring = ring.clone();
    let producer = std::thread::spawn(move || {
        for i in 0..1_000_000 {
            while !producer_ring.push(i as u8) {
                std::hint::spin_loop();
            }
        }
    });
    for i in 0..1_000_000 {
        loop {
            if let Some(byte) = ring.pop() {
                assert_eq!(byte, i as u8);
                break;
            }
            std::hint::spin_loop();
        }
    }
    producer.join().unwrap();
    assert!(!ring.has_data());
}
