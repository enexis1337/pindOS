#!/usr/bin/env python3
"""Real echo-test via drun/PTY, or five post-commit ping runs on the current ISO."""
from pathlib import Path
import argparse
import hashlib
import json
import os
import pty
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent.parent
PING = b"[ping] finished: 4 sent, 4 replied, 0 timed out"
READY = b"[echo-test] waiting for input"


def check_guest(output):
    for marker in (b"PANIC", b"[echo-test] exited", b"FAILED to start", b"zero sized"):
        if marker in output:
            raise RuntimeError(f"guest failure: {marker!r}")


def record_artifacts(work):
    records = {"commit": subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(), "sha256": {}}
    for name in ("hammam.iso", "hammam/target/x86_64-unknown-none/debug/hammam-kernel",
                 "userspace/echo-test/target/x86_64-unknown-none/release/echo-test"):
        path = ROOT / name
        records["sha256"][name] = hashlib.sha256(path.read_bytes()).hexdigest()
    (work / "artifacts.json").write_text(json.dumps(records, indent=2) + "\n")


def interactive(work):
    stdout = work / "drun.stdout.log"
    stderr = work / "drun.stderr.log"
    master, slave = pty.openpty()
    with stdout.open("wb") as out, stderr.open("wb") as err:
        process = subprocess.Popen(
            [sys.executable, str(ROOT / "drun.py"), "-T"], cwd=ROOT,
            stdin=slave, stdout=out, stderr=err, start_new_session=True,
        )
        os.close(slave)

        def wait_for(marker, timeout=40):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                output = stdout.read_bytes()
                check_guest(output)
                if process.poll() is not None:
                    raise RuntimeError(f"drun exited: {process.returncode}")
                if marker in output:
                    return
                time.sleep(0.02)
            raise RuntimeError(f"timeout waiting for {marker!r}; see {work}")

        try:
            wait_for(READY)
            wait_for(PING)
            print("[OK] ping 4/4 before any stdin was sent to echo-test", flush=True)
            with (work / "stdin.bin").open("wb") as raw, (work / "input.log").open("w") as inputs:
                cases = [
                    (b"PINDOS echo test\r", b"PINDOS echo test"),
                    (b"second line\n", b"second line"),
                    (b"third line\r\n", b"third line"),
                    (b"x" * 512 + b"\r", b"x" * 256),
                    (b"after long line\r", b"after long line"),
                    (b"\r", b""),
                ]
                inputs.write("All input below was sent AFTER the complete ping 4/4 line.\n")
                for index, (payload, expected) in enumerate(cases, 1):
                    inputs.write(f"stdin bytes: {payload!r}\n")
                    inputs.flush()
                    raw.write(payload)
                    raw.flush()
                    # Host-side typing pace only: do not overflow the 16-byte UART
                    # FIFO. No device-init delays or kernel timing modifications.
                    for offset in range(0, len(payload), 8):
                        block = payload[offset:offset + 8]
                        assert os.write(master, block) == len(block)
                        time.sleep(0.01)
                    wait_for(b"[USERSPACE] [echo] " + expected + b"\n")
                    echoes = [line for line in stdout.read_bytes().splitlines()
                              if line.startswith(b"[USERSPACE] [echo] ")]
                    assert len(echoes) == index, echoes
                    assert echoes[-1] == b"[USERSPACE] [echo] " + expected
                    print(f"[OK] echo case {index}: input={len(payload)} bytes, "
                          f"line={len(expected)} bytes", flush=True)
            time.sleep(0.2)
            output = stdout.read_bytes()
            check_guest(output)
            assert output.count(READY) == 1, "echo-test restarted"
            assert output.count(b"[syscall] exec: /usr/bin/echo-test") == 1
            assert b"[syscall] exit(" not in output, "service exited"
            assert b"matches ELF" in output
            record_artifacts(work)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            os.close(master)
            data = stdout.read_bytes()
            start = data.find(b"Hammam / PINDOS booting...")
            if start >= 0:
                (work / "com1.log").write_bytes(data[start:])
            print("--- raw drun stdout (includes COM1) ---", flush=True)
            print(data.decode(errors="replace"), end="", flush=True)
            print("--- raw drun stderr (build + QEMU) ---", flush=True)
            print(stderr.read_text(errors="replace"), end="", flush=True)
    qemu_lines = [line for line in stderr.read_text().splitlines()
                  if line.startswith("qemu-system-x86_64:") or "virtio:" in line]
    assert qemu_lines and all("terminating on signal 15" in line for line in qemu_lines)
    print("[OK] drun/PTY echo regression", flush=True)


def ping_runs(work):
    record_artifacts(work)
    for number in range(1, 6):
        com1 = work / f"ping-{number}.com1.log"
        stderr = work / f"ping-{number}.stderr.log"
        with com1.open("wb") as out, stderr.open("wb") as err:
            result = subprocess.run(
                ["timeout", "8s", "qemu-system-x86_64", "-M", "pc",
                 "-cdrom", str(ROOT / "hammam.iso"), "-serial", "stdio",
                 "-monitor", "none", "-display", "none", "-m", "256M",
                 "-netdev", "user,id=net0", "-device", "virtio-net-pci,netdev=net0",
                 "-no-reboot"], stdin=subprocess.DEVNULL, stdout=out, stderr=err,
            )
        output = com1.read_bytes()
        diagnostics = stderr.read_bytes()
        print(f"--- ping run {number}/5, exit={result.returncode}: raw COM1 ---", flush=True)
        print(output.decode(errors="replace"), end="", flush=True)
        print("--- raw QEMU stderr ---", flush=True)
        print(diagnostics.decode(errors="replace"), end="", flush=True)
        # Fail immediately: retain all files and do not continue to the next run.
        check_guest(output)
        if result.returncode != 124 or PING not in output:
            raise RuntimeError(f"ping regression in run {number}; compare against 1a77598")
        assert READY in output and output.count(READY) == 1
        assert b"[syscall] exit(" not in output
        assert diagnostics.strip() and all(b"terminating on signal 15" in line
                                          for line in diagnostics.splitlines())
    print("[OK] post-commit ping 5/5, echo-test waiting without input", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--ping-only", action="store_true")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    print(f"Artifacts: {args.output.resolve()}", flush=True)
    if args.ping_only:
        ping_runs(args.output)
    else:
        interactive(args.output)


if __name__ == "__main__":
    main()
