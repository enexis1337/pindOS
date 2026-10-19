use alloc::vec::Vec;

/// Dealduck — PID 1, сервис-менеджер PINDOS
pub const DEALDUCK_ELF: &[u8] = include_bytes!(
    "../../dealduck/target/x86_64-unknown-none/release/dealduck"
);

/// Net-server — сетевой стек, запускается dealduck как сервис
pub const NET_SERVER_ELF: &[u8] = include_bytes!(
    "../../userspace/net-server/target/x86_64-unknown-none/release/net-server"
);

/// Echo-test — блокирующее чтение консоли, запускается рядом с net-server.
pub const ECHO_TEST_ELF: &[u8] = include_bytes!(
    "../../userspace/echo-test/target/x86_64-unknown-none/release/echo-test"
);

/// systemd-стиль unit-файл для net-server
pub const NET_SERVER_SERVICE: &[u8] = b"[Unit]\nDescription=PINDOS Network Stack\n\n[Service]\nType=simple\nExecStart=/usr/bin/net-server\nRestart=on-failure\n";

/// Build a CPIO newc archive containing /init, net-server and echo-test.
pub fn build_initramfs() -> Vec<u8> {
    let mut cpio = Vec::new();
    append_cpio_entry(&mut cpio, b"/init", DEALDUCK_ELF);
    append_cpio_entry(&mut cpio, b"/usr/bin/net-server", NET_SERVER_ELF);
    append_cpio_entry(&mut cpio, b"/usr/bin/echo-test", ECHO_TEST_ELF);
    append_cpio_entry(&mut cpio, b"/etc/pindos/system/net-server.service", NET_SERVER_SERVICE);
    append_cpio_trailer(&mut cpio);
    cpio
}

fn append_cpio_entry(cpio: &mut Vec<u8>, name: &[u8], data: &[u8]) {
    let name_with_nul = &[name, &[0]].concat();
    // magic "070701" (6) + ino(8) + mode(8) + uid(8) + gid(8)
    cpio.extend_from_slice(b"07070100000000");
    // mode: regular file 0100644 = 0x81A4
    cpio.extend_from_slice(b"000081a4");
    // uid, gid, nlink, mtime
    cpio.extend_from_slice(b"00000000000000000000000100000000");
    // filesize (8 hex chars)
    cpio.extend_from_slice(&hex8(data.len()));
    // devmajor, devminor, rdevmajor, rdevminor
    cpio.extend_from_slice(b"00000000000000000000000000000000");
    // namesize (8 hex chars)
    cpio.extend_from_slice(&hex8(name_with_nul.len()));
    // check
    cpio.extend_from_slice(b"00000000");
    // filename
    cpio.extend_from_slice(name_with_nul);
    align4(cpio);
    // file data
    cpio.extend_from_slice(data);
    align4(cpio);
}

fn append_cpio_trailer(cpio: &mut Vec<u8>) {
    append_cpio_entry(cpio, b"TRAILER!!!", &[]);
}

fn hex8(v: usize) -> [u8; 8] {
    let mut buf = [0u8; 8];
    for i in 0..8 {
        let nibble = (v >> (4 * (7 - i))) & 0xF;
        buf[i] = if nibble < 10 { b'0' + nibble as u8 } else { b'a' + nibble as u8 - 10 };
    }
    buf
}

fn align4(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}