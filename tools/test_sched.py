#!/usr/bin/env python3
"""Run the kernel-only blocking test without changing the working boot.rs/ISO."""
from pathlib import Path
import os
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def main():
    # The feature changes .text/.rodata size and therefore BOOT_WORKSPACE.
    # Keep address fixups, Cargo output, ISO and logs separate from normal builds.
    work = Path(tempfile.mkdtemp(prefix="pindos-sched-test-"))
    print(f"Artifacts: {work}", flush=True)
    kernel = work / "hammam"
    shutil.copytree(ROOT / "hammam/src", kernel / "src")
    shutil.copytree(ROOT / "hammam/.cargo", kernel / ".cargo")
    for name in ("Cargo.toml", "Cargo.lock", "link.ld"):
        source = ROOT / "hammam" / name
        if source.exists():
            shutil.copy2(source, kernel / name)
    for name in ("dealduck", "userspace"):
        (work / name).symlink_to(ROOT / name, target_is_directory=True)
    env = dict(os.environ, CARGO_TARGET_DIR=str(work / "target"))
    elf = work / "target/x86_64-unknown-none/debug/hammam-kernel"
    boot = kernel / "src/boot.rs"
    for _ in range(3):
        with (work / "build.log").open("w") as log:
            subprocess.run(
                ["cargo", "build", "--target", "x86_64-unknown-none", "--features", "sched-test"],
                cwd=kernel, env=env, stdout=log, stderr=subprocess.STDOUT, check=True,
            )
        symbols = subprocess.check_output(["nm", str(elf)], text=True)
        address = int(re.search(r"([0-9a-f]+)\s+\w\s+\S*BOOT_WORKSPACE\S*", symbols)[1], 16)
        source = boot.read_text()
        declared = re.search(r"DATA_BOOT_VADDR: u32 = (0x[0-9a-fA-F]+)", source)[1]
        if int(declared, 16) == address:
            break
        boot.write_text(source.replace(declared, hex(address)))
    else:
        raise RuntimeError("BOOT_WORKSPACE address did not settle; see build.log")
    print(f"[OK] TEST DATA_BOOT_VADDR = {address:#x} matches ELF", flush=True)

    grub = work / "iso/boot/grub"
    grub.mkdir(parents=True)
    shutil.copy2(elf, work / "iso/boot/hammam.elf")
    (grub / "grub.cfg").write_text(
        'set timeout=0\nset default=0\nmenuentry "sched-test" {\n'
        ' multiboot2 /boot/hammam.elf\n boot\n}\n'
    )
    iso = work / "sched-test.iso"
    with (work / "iso.log").open("w") as log:
        subprocess.run(
            ["grub-mkrescue", "-o", str(iso), str(work / "iso")],
            stdout=log, stderr=subprocess.STDOUT, check=True,
        )
    for number in range(1, 4):
        com1 = work / f"run-{number}.com1"
        stderr = work / f"run-{number}.stderr"
        with com1.open("w") as out, stderr.open("w") as err:
            result = subprocess.run(
                ["timeout", "6s", "qemu-system-x86_64", "-M", "pc", "-cdrom", str(iso),
                 "-serial", "stdio", "-display", "none", "-m", "256M", "-no-reboot"],
                stdout=out, stderr=err,
            )
        output = com1.read_text()
        diagnostics = stderr.read_text()
        print(f"--- kernel test {number}, exit={result.returncode}: COM1 ---", flush=True)
        print(output, end="", flush=True)
        print("--- QEMU stderr ---", flush=True)
        print(diagnostics, end="", flush=True)
        if (result.returncode != 124 or "[block-test] ALL PASS" not in output
                or "B: resumed from IRQ wake" not in output or "PANIC" in output
                or any("terminating on signal 15" not in line for line in diagnostics.splitlines())):
            raise RuntimeError(f"kernel test {number} failed; artifacts: {work}")
    print("[OK] kernel-only blocking tests: 3/3", flush=True)


if __name__ == "__main__":
    main()
