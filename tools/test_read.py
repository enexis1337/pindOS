#!/usr/bin/env python3
"""Exercise real sys_read in an isolated ISO, leaving normal sources/ISO alone."""
from pathlib import Path
import os
import re
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent


def main():
    work = Path(tempfile.mkdtemp(prefix="pindos-read-test-"))
    print(f"Artifacts: {work}", flush=True)
    kernel = work / "hammam"
    init = work / "dealduck"
    probe = work / "probe"
    for source, dest in ((ROOT / "hammam", kernel), (ROOT / "dealduck", init)):
        shutil.copytree(source / "src", dest / "src")
        shutil.copytree(source / ".cargo", dest / ".cargo")
        for name in ("Cargo.toml", "Cargo.lock", "link.ld"):
            if (source / name).exists():
                shutil.copy2(source / name, dest / name)
    (work / "userspace").symlink_to(ROOT / "userspace", target_is_directory=True)
    (probe / "src").mkdir(parents=True)
    shutil.copytree(ROOT / "dealduck/.cargo", probe / ".cargo")
    shutil.copy2(ROOT / "tools/read_probe.rs", probe / "src/main.rs")
    (probe / "Cargo.toml").write_text(
        '[package]\nname="read-probe"\nversion="0.1.0"\nedition="2021"\n'
        '[profile.release]\npanic="abort"\nopt-level=2\n'
    )

    def build(path, release=False):
        env = dict(os.environ, CARGO_TARGET_DIR=str(path / "target"))
        with (work / f"{path.name}-build.log").open("a") as log:
            subprocess.run(
                ["cargo", "build", "--target", "x86_64-unknown-none"]
                + (["--release"] if release else []),
                cwd=path, env=env, stdout=log, stderr=subprocess.STDOUT, check=True,
            )

    wake_probe = work / "wake-probe"
    shutil.copytree(probe / "src", wake_probe / "src")
    shutil.copytree(probe / ".cargo", wake_probe / ".cargo")
    (wake_probe / "Cargo.toml").write_text(
        '[package]\nname="wake-probe"\nversion="0.1.0"\nedition="2021"\n'
        '[features]\ndefault=["broadcast"]\nbroadcast=[]\n'
        '[profile.release]\npanic="abort"\nopt-level=2\n'
    )
    build(probe, release=True)
    build(wake_probe, release=True)
    main_rs = init / "src/main.rs"
    original = '    manager.register("net-server", "/usr/bin/net-server");'
    source = main_rs.read_text()
    assert original in source
    main_rs.write_text(source.replace(original, original +
        '\n    manager.register("read-probe", "/usr/bin/read-probe");'))
    build(init, release=True)
    blob = kernel / "src/userspace_blob.rs"
    source = blob.read_text()
    original = '    append_cpio_trailer(&mut cpio);'
    assert original in source
    probe_elf = probe / "target/x86_64-unknown-none/release/read-probe"
    wake_elf = wake_probe / "target/x86_64-unknown-none/release/wake-probe"
    blob.write_text(source.replace(original,
        f'    append_cpio_entry(&mut cpio, b"/usr/bin/read-probe", '
        f'include_bytes!("{probe_elf}"));\n'
        f'    append_cpio_entry(&mut cpio, b"/usr/bin/wake-probe", '
        f'include_bytes!("{wake_elf}"));\n' + original))
    elf = kernel / "target/x86_64-unknown-none/debug/hammam-kernel"
    boot = kernel / "src/boot.rs"
    for _ in range(3):
        build(kernel)
        symbols = subprocess.check_output(["nm", str(elf)], text=True)
        address = int(re.search(r"([0-9a-f]+)\s+\w\s+\S*BOOT_WORKSPACE\S*", symbols)[1], 16)
        source = boot.read_text()
        declared = re.search(r"DATA_BOOT_VADDR: u32 = (0x[0-9a-fA-F]+)", source)[1]
        if int(declared, 16) == address:
            break
        boot.write_text(source.replace(declared, hex(address)))
    else:
        raise RuntimeError("BOOT_WORKSPACE address did not settle")
    print(f"[OK] TEST DATA_BOOT_VADDR = {address:#x} matches ELF", flush=True)
    grub = work / "iso/boot/grub"
    grub.mkdir(parents=True)
    shutil.copy2(elf, work / "iso/boot/hammam.elf")
    (grub / "grub.cfg").write_text(
        'set timeout=0\nset default=0\nmenuentry "read-test" {\n'
        ' multiboot2 /boot/hammam.elf\n boot\n}\n'
    )
    iso = work / "read-test.iso"
    with (work / "iso.log").open("w") as log:
        subprocess.run(["grub-mkrescue", "-o", str(iso), str(work / "iso")],
                       stdout=log, stderr=subprocess.STDOUT, check=True)

    com1 = work / "com1.log"
    stderr = work / "qemu.stderr"
    with com1.open("w") as out, stderr.open("w") as err:
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-M", "pc", "-cdrom", str(iso),
             "-serial", "stdio", "-monitor", "none", "-display", "none", "-m", "256M",
             "-netdev", "user,id=net0", "-device", "virtio-net-pci,netdev=net0", "-no-reboot"],
            stdin=subprocess.PIPE, stdout=out, stderr=err,
        )

        def wait_for(marker):
            deadline = time.monotonic() + 25
            while time.monotonic() < deadline:
                output = com1.read_text()
                if "PANIC" in output or "[read-probe] FAIL" in output or qemu.poll() is not None:
                    raise RuntimeError(f"guest failed waiting for {marker!r}")
                if marker in output:
                    return
                time.sleep(0.02)
            raise RuntimeError(f"timeout waiting for {marker!r}")

        try:
            wait_for("[read-probe] WAIT chunked input")
            # No input until all pings complete: the real reader must be asleep.
            wait_for("[ping] finished: 4 sent, 4 replied, 0 timed out")
            print("[OK] net-server ping 4/4 while sys_read is waiting", flush=True)
            print("COM1 input bytes: b'abcdef'", flush=True)
            qemu.stdin.write(b"abcdef")
            qemu.stdin.flush()
            wait_for("[read-probe] WAIT one byte (len=16)")
            time.sleep(0.1)
            print("COM1 input bytes: b'Z'", flush=True)
            qemu.stdin.write(b"Z")
            qemu.stdin.flush()
            wait_for("[read-probe] ALL PASS")
            deadline = time.monotonic() + 10
            while com1.read_text().count("[wake-probe] WAIT") != 2:
                if time.monotonic() > deadline or qemu.poll() is not None:
                    raise RuntimeError("two readers did not start")
                time.sleep(0.02)
            time.sleep(0.1)
            print("COM1 input bytes: b'A' (wake all, only one can read)", flush=True)
            qemu.stdin.write(b"A")
            qemu.stdin.flush()
            wait_for("[wake-probe] GOT A")
            time.sleep(0.2)
            assert com1.read_text().count("[wake-probe] GOT") == 1
            print("COM1 input bytes: b'B' (remaining reader must have reblocked)", flush=True)
            qemu.stdin.write(b"B")
            qemu.stdin.flush()
            wait_for("[wake-probe] GOT B")
            assert com1.read_text().count("[wake-probe] GOT") == 2
            print("[OK] wake-all readers rechecked condition and consumed A/B once", flush=True)
        finally:
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()
                qemu.wait()
            qemu.stdin.close()
            print("--- raw COM1 ---", flush=True)
            print(com1.read_text(), end="", flush=True)
            print("--- QEMU stderr ---", flush=True)
            print(stderr.read_text(), end="", flush=True)
    diagnostics = stderr.read_text().splitlines()
    if any("terminating on signal 15" not in line for line in diagnostics):
        raise RuntimeError("unexpected QEMU stderr")
    print("[OK] sys_read userspace regression", flush=True)


if __name__ == "__main__":
    main()
