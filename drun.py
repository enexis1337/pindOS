#!/usr/bin/env python3
"""
dRun — тулчейн сборки PINDOS / Hammam kernel.

Использование:
  python drun.py -get            # установить все зависимости
  python drun.py -b              # собрать debug
  python drun.py -c              # cargo check всех компонентов
  python drun.py -T              # собрать + запустить QEMU с отладкой
  python drun.py -r 0.1-moorino  # собрать release ISO для реального железа
"""

import argparse
import subprocess
import shutil
import sys
import os
from pathlib import Path
from datetime import datetime

# Построчная буферизация: при перенаправлении в пайп (`python drun.py -T | ...`)
# stdout по умолчанию блокируется, и наши print() застревают в буфере, тогда как
# QEMU пишет в тот же fd напрямую. Из-за этого COM1-вывод терялся.
try:
    sys.stdout.reconfigure(line_buffering=True)
    sys.stderr.reconfigure(line_buffering=True)
except AttributeError:
    pass

# ── Пути ──────────────────────────────────────────────────────────────────────
ROOT        = Path(__file__).parent.resolve()
DRUNNED     = ROOT / "drunned"
HAMMAM_DIR  = ROOT / "hammam"
TOOLS_DIR   = ROOT / "tools"

KERNEL_DEBUG   = HAMMAM_DIR / "target/x86_64-unknown-none/debug/hammam-kernel"
KERNEL_RELEASE = HAMMAM_DIR / "target/x86_64-unknown-none/release/hammam-kernel"
ISO_PATH       = ROOT / "hammam.iso"

TARGET = "x86_64-unknown-none"

# ── Утилиты ───────────────────────────────────────────────────────────────────
def banner(text: str):
    line = "=" * 60
    print(f"\n{line}")
    print(f"  {text}")
    print(f"{line}\n")

def run(cmd: list[str], cwd: Path = ROOT, check: bool = True) -> int:
    print(f"  $ {' '.join(str(c) for c in cmd)}")
    result = subprocess.run(cmd, cwd=cwd, check=False)
    if check and result.returncode != 0:
        print(f"\n[FAIL] команда завершилась с кодом {result.returncode}")
        sys.exit(result.returncode)
    return result.returncode

def sh(cmd: str, check: bool = True) -> int:
    """Выполнить shell-команду через bash."""
    return run(["bash", "-c", cmd], check=check)

def check_tool(name: str) -> bool:
    """Проверить наличие утилиты в PATH."""
    return shutil.which(name) is not None

# ── Установка зависимостей ─────────────────────────────────────────────────────
def cmd_get():
    """-get : установить все зависимости для сборки ядра и ОС."""
    banner("dRun GET — установка зависимостей")

    # ── 1. Определить пакетный менеджер ───────────────────────────────────────
    if check_tool("apt-get"):
        pkg_mgr = "apt"
    elif check_tool("dnf"):
        pkg_mgr = "dnf"
    elif check_tool("pacman"):
        pkg_mgr = "pacman"
    else:
        print("[FAIL] Не удалось определить пакетный менеджер (apt/dnf/pacman).")
        print("       Установите зависимости вручную — список выведен ниже.")
        _print_deps_manual()
        sys.exit(1)

    print(f"  Пакетный менеджер: {pkg_mgr}\n")

    # ── 2. Системные пакеты ───────────────────────────────────────────────────
    #   grub-pc-bin / grub2-pc        — grub-mkrescue (создание ISO)
    #   xorriso                       — ISO 9660 backend для grub-mkrescue
    #   qemu-system-x86               — эмулятор для тестирования
    #   binutils / llvm               — objcopy для патча ELF OS/ABI
    #   gcc / build-essential         — линковщик и libc (нужны cargo для host-утилит)
    #   cpio                          — сборка initramfs
    #   nasm                          — ассемблер (на случай отдельных .asm файлов)
    #   curl                          — установщик rustup

    pkg_map = {
        "apt": [
            "build-essential",
            "gcc",
            "binutils",
            "llvm",
            "grub-pc-bin",
            "grub-efi-amd64-bin",
            "xorriso",
            "qemu-system-x86",
            "cpio",
            "nasm",
            "curl",
        ],
        "dnf": [
            "gcc",
            "binutils",
            "llvm",
            "grub2-pc",
            "grub2-efi-x64",
            "xorriso",
            "qemu-system-x86",
            "cpio",
            "nasm",
            "curl",
        ],
        "pacman": [
            "base-devel",
            "gcc",
            "binutils",
            "llvm",
            "grub",
            "xorriso",
            "qemu-system-x86",
            "cpio",
            "nasm",
            "curl",
        ],
    }

    install_cmds = {
        "apt":    ["sudo", "apt-get", "install", "-y"],
        "dnf":    ["sudo", "dnf",     "install", "-y"],
        "pacman": ["sudo", "pacman",  "-S", "--noconfirm"],
    }

    pkgs = pkg_map[pkg_mgr]

    print("  [1/4] Установка системных пакетов...")
    if pkg_mgr == "apt":
        run(["sudo", "apt-get", "update", "-y"], check=False)
    run(install_cmds[pkg_mgr] + pkgs)

    # ── 3. Rust + rustup ──────────────────────────────────────────────────────
    print("\n  [2/4] Проверка Rust / rustup...")
    if check_tool("rustup"):
        print("  rustup уже установлен, обновляем...")
        run(["rustup", "update"])
    else:
        print("  Устанавливаем rustup...")
        sh("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path")
        # Добавить cargo в PATH для текущего сеанса
        cargo_bin = Path.home() / ".cargo" / "bin"
        os.environ["PATH"] = str(cargo_bin) + ":" + os.environ.get("PATH", "")
        print(f"\n  Cargo bin добавлен в PATH сеанса: {cargo_bin}")
        print("  Не забудьте добавить в ~/.bashrc или ~/.zshrc:")
        print(f'    export PATH="$HOME/.cargo/bin:$PATH"')

    # ── 4. Rust toolchain и таргет ────────────────────────────────────────────
    print("\n  [3/4] Установка Rust nightly + target x86_64-unknown-none...")
    run(["rustup", "toolchain", "install", "nightly"])
    run(["rustup", "override",  "set",     "nightly"],   cwd=HAMMAM_DIR)
    run(["rustup", "target",    "add",     TARGET],      cwd=HAMMAM_DIR)
    run(["rustup", "component", "add",     "rust-src"],  cwd=HAMMAM_DIR)
    run(["rustup", "component", "add",     "llvm-tools-preview"], cwd=HAMMAM_DIR)

    # ── 5. Проверка итога ─────────────────────────────────────────────────────
    print("\n  [4/4] Проверка установленных инструментов...")
    tools = [
        ("rustc",             "Rust compiler"),
        ("cargo",             "Cargo"),
        ("grub-mkrescue",     "grub-mkrescue"),
        ("xorriso",           "xorriso"),
        ("qemu-system-x86_64","QEMU"),
        ("objcopy",           "objcopy (binutils)"),
        ("cpio",              "cpio"),
        ("nasm",              "nasm"),
    ]

    all_ok = True
    for tool, label in tools:
        found = check_tool(tool)
        status = "[OK]  " if found else "[MISS]"
        if not found:
            all_ok = False
        print(f"    {status} {label:<30} ({tool})")

    print()
    if all_ok:
        print("[OK] Все зависимости установлены. Можно собирать: python drun.py -b")
    else:
        print("[WARN] Некоторые инструменты не найдены — см. выше.")
        print("       Возможно, нужно открыть новый терминал (PATH обновится).")

def _print_deps_manual():
    """Вывести список зависимостей для ручной установки."""
    print("""
  Системные пакеты (названия для apt/debian):
    build-essential gcc binutils llvm
    grub-pc-bin grub-efi-amd64-bin xorriso
    qemu-system-x86 cpio nasm curl

  Rust:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
    rustup toolchain install nightly
    rustup target add x86_64-unknown-none
    rustup component add rust-src llvm-tools-preview
""")

def ensure_drunned():
    DRUNNED.mkdir(exist_ok=True)

def copy_artifact(src: Path, dest_name: str):
    ensure_drunned()
    dest = DRUNNED / dest_name
    shutil.copy2(src, dest)
    size = dest.stat().st_size / 1024
    print(f"  → drunned/{dest_name} ({size:.1f} KiB)")

def sync_boot_workspace_addr() -> bool:
    """Синхронизировать DATA_BOOT_VADDR с реальным адресом BOOT_WORKSPACE.

    Секция .data ядра сдвигается при каждом изменении размера встроенного
    ELF net-server (он лежит в .rodata через include_bytes!). Константа в
    boot.rs от этого уезжает, и ядро падает мгновенно, до единого print.
    Возвращает True, если константу пришлось править.
    """
    import subprocess, re

    if not KERNEL_DEBUG.exists():
        return False

    nm_out = subprocess.check_output(["nm", str(KERNEL_DEBUG)], text=True)
    m = re.search(r'([0-9a-f]+)\s+\w\s+\S*BOOT_WORKSPACE\S*', nm_out)
    if not m:
        print("  [WARN] BOOT_WORKSPACE symbol not found in ELF")
        return False

    real = int(m.group(1), 16)
    boot_rs_path = ROOT / "hammam/src/boot.rs"
    boot_rs = boot_rs_path.read_text()
    c = re.search(r'DATA_BOOT_VADDR:\s*u32\s*=\s*(0x[0-9a-fA-F]+)', boot_rs)
    if not c:
        print("  [WARN] DATA_BOOT_VADDR not found in boot.rs")
        return False

    if int(c.group(1), 16) == real:
        return False

    print(f"  [FIX] DATA_BOOT_VADDR {c.group(1)} -> {real:#x}, пересобираю ядро")
    boot_rs_path.write_text(boot_rs.replace(c.group(1), f"{real:#x}"))
    run(["cargo", "build", "--target", TARGET], cwd=HAMMAM_DIR, check=False)
    return True


def rebuild_iso_forced():
    """Принудительная пересборка ISO: снести образ и промежуточный каталог."""
    for stale in (ROOT / "hammam.iso", ROOT / "hammam.iso.new"):
        try:
            stale.unlink()
        except FileNotFoundError:
            pass
    iso_root = ROOT / "iso_root/boot"
    if iso_root.exists():
        shutil.rmtree(iso_root)
    sh("bash tools/make_iso.sh")


def check_boot_workspace_addr():
    """Проверить что DATA_BOOT_VADDR в boot.rs совпадает с реальным адресом BOOT_WORKSPACE."""
    import subprocess, re

    kernel_elf = KERNEL_DEBUG
    if not kernel_elf.exists():
        return

    nm_out = subprocess.check_output(["nm", str(kernel_elf)], text=True)
    match = re.search(r'([0-9a-f]+)\s+\w\s+\S*BOOT_WORKSPACE\S*', nm_out)
    if not match:
        print("  [WARN] BOOT_WORKSPACE symbol not found in ELF")
        return

    real_addr = int(match.group(1), 16)
    boot_rs = (ROOT / "hammam/src/boot.rs").read_text()
    const_match = re.search(r'DATA_BOOT_VADDR:\s*u32\s*=\s*(0x[0-9a-fA-F]+)', boot_rs)
    if not const_match:
        print("  [WARN] DATA_BOOT_VADDR not found in boot.rs")
        return

    declared_addr = int(const_match.group(1), 16)
    if real_addr != declared_addr:
        print(f"\n  [FATAL] DATA_BOOT_VADDR MISMATCH!")
        print(f"          boot.rs declares: {declared_addr:#x}")
        print(f"          ELF real address: {real_addr:#x}")
        print(f"          Fix: update DATA_BOOT_VADDR in boot.rs to {real_addr:#x}")
        sys.exit(1)
    else:
        print(f"  [OK] DATA_BOOT_VADDR = {real_addr:#x} matches ELF")

# ── Команды ───────────────────────────────────────────────────────────────────
def cmd_check():
    """-c : cargo check всех компонентов."""
    banner("dRun CHECK")
    
    components = [
        (HAMMAM_DIR, "Hammam kernel", True),  # True = use x86_64-unknown-none target
    ]
    
    # Добавить userspace компоненты  
    # net-server требует Linux для сборки (используется x86_64-unknown-linux-gnu)
    for name in ["userspace/hello"]:
        path = ROOT / name
        if (path / "Cargo.toml").exists():
            components.append((path, name, True))
    
    all_ok = True
    for path, label, use_custom_target in components:
        print(f"  Checking {label}...")
        
        if use_custom_target:
            args = ["cargo", "check", "--target", TARGET]
        else:
            args = ["cargo", "check"]
        
        result = subprocess.run(
            args,
            cwd=path, 
            check=False,
            capture_output=False
        )
        
        if result.returncode != 0:
            print(f"  [FAIL] {label}")
            all_ok = False
        else:
            print(f"  [OK]   {label}")
    
    print(f"\n  Note: userspace/net-server skipped (requires Linux host)")
    
    if all_ok:
        print("\n[OK] Все компоненты прошли проверку.")
    else:
        print("\n[FAIL] Есть ошибки — см. выше.")
        sys.exit(1)

def cmd_build():
    """-b : собрать debug сборку."""
    banner("dRun BUILD (debug)")
    
    print("  Сборка Hammam kernel...")
    run(["cargo", "build", "--target", TARGET], cwd=HAMMAM_DIR)
    check_boot_workspace_addr()
    
    print("\n  Создание ISO...")
    sh("bash tools/make_iso.sh")
    
    ensure_drunned()
    copy_artifact(KERNEL_DEBUG, "hammam-kernel-debug")
    copy_artifact(ISO_PATH, "pindos-debug.iso")
    
    print("\n[OK] Debug сборка готова → drunned/")

def cmd_test():
    """-T : собрать + запустить QEMU с отладочным выводом."""
    banner("dRun TEST (QEMU debug)")
    
    # Порядок важен: userspace-бинари встраиваются в ядро через include_bytes!,
    # поэтому собираем их ДО ядра, иначе ядро утащит устаревшие ELF.
    print("  Сборка userspace (встраивается в ядро)...")
    for d in ("userspace/net-server", "dealduck"):
        run(["cargo", "build", "--release", "--target", TARGET],
            cwd=ROOT / d, check=False)

    print("  Сборка Hammam kernel...")
    run(["cargo", "build", "--target", TARGET], cwd=HAMMAM_DIR)

    # .data сдвигается при изменении встроенных ELF, константа за это не знает.
    sync_boot_workspace_addr()
    check_boot_workspace_addr()

    print("  Создание ISO (принудительно)...")
    rebuild_iso_forced()
    copy_artifact(ISO_PATH, "pindos-debug.iso")
    
    # Запустить QEMU
    banner("QEMU — вывод ядра (COM1)")
    print("  Для остановки: Ctrl+C\n")
    
    qemu_cmd = [
        "qemu-system-x86_64",
        "-M", "pc",
        "-cdrom", str(ISO_PATH),
        "-serial", "stdio",
        "-display", "none",
        "-m", "256M",
        "-netdev", "user,id=net0,hostfwd=tcp::8080-:80",
        "-device", "virtio-net-pci,netdev=net0",
        "-d", "int,cpu_reset",        # отладка: прерывания и CPU reset
        "-D", str(DRUNNED / "qemu-debug.log"),  # лог в файл
        "-no-reboot",                  # не перезагружаться при краше
    ]
    
    print(f"  $ {' '.join(qemu_cmd)}\n")
    print("─" * 60)
    
    try:
        subprocess.run(qemu_cmd, check=False)
    except KeyboardInterrupt:
        pass
    
    log = DRUNNED / "qemu-debug.log"
    if log.exists() and log.stat().st_size > 0:
        print(f"\n{'─' * 60}")
        print(f"  Отладочный лог QEMU → drunned/qemu-debug.log")
        print(f"  Последние 20 строк:")
        lines = log.read_text(errors="replace").splitlines()
        for line in lines[-20:]:
            print(f"    {line}")

def cmd_release(version: str):
    """−r <version> : release ISO для реального железа."""
    banner(f"dRun RELEASE v{version}")
    
    timestamp = datetime.now().strftime("%Y%m%d-%H%M")
    iso_name  = f"pindos-{version}-{timestamp}.iso"
    kernel_name = f"hammam-kernel-{version}"
    
    print("  Сборка Hammam kernel (release)...")
    run(["cargo", "build", "--target", TARGET, "--release"], cwd=HAMMAM_DIR)
    
    # Создать ISO из release бинаря
    print("  Создание release ISO...")
    sh(f"KERNEL_PATH=hammam/target/x86_64-unknown-none/release/hammam-kernel "
       f"bash tools/make_iso.sh")
    
    ensure_drunned()
    copy_artifact(KERNEL_RELEASE, kernel_name)
    copy_artifact(ISO_PATH, iso_name)
    
    # Записать метаданные релиза
    meta = DRUNNED / f"pindos-{version}-{timestamp}.txt"
    meta.write_text(
        f"PINDOS Release\n"
        f"Version:   {version}\n"
        f"Built:     {datetime.now().isoformat()}\n"
        f"Kernel:    {kernel_name}\n"
        f"ISO:       {iso_name}\n"
        f"Target:    {TARGET}\n"
    )
    
    print(f"\n[OK] Release готов:")
    print(f"     drunned/{iso_name}")
    print(f"     drunned/{kernel_name}")
    print(f"     drunned/{meta.name}")
    print(f"\n  Запись на флешку (пример):")
    print(f"     dd if=drunned/{iso_name} of=/dev/sdX bs=4M status=progress")

def cmd_clean():
    """-cl : очистить все артефакты сборки."""
    banner("dRun CLEAN")
    
    items_to_clean = []
    
    # Cargo target директории
    cargo_projects = [
        HAMMAM_DIR,
        ROOT / "userspace" / "hello",
        ROOT / "userspace" / "net-server",
        ROOT / "userspace" / "nvme-driver",
        ROOT / "userspace" / "dealduck",
        ROOT / "userspace" / "posix-compat",
    ]
    
    for project in cargo_projects:
        target_dir = project / "target"
        if target_dir.exists():
            items_to_clean.append((target_dir, f"{project.name}/target"))
    
    # ISO и временные файлы
    if ISO_PATH.exists():
        items_to_clean.append((ISO_PATH, "hammam.iso"))
    
    iso_new = ROOT / "hammam.iso.new"
    if iso_new.exists():
        items_to_clean.append((iso_new, "hammam.iso.new"))
    
    iso_root = ROOT / "iso_root"
    if iso_root.exists():
        items_to_clean.append((iso_root, "iso_root/"))
    
    # Логи
    logs = [
        ROOT / "serial.log",
        ROOT / "qemu_debug.log",
    ]
    for log in logs:
        if log.exists():
            items_to_clean.append((log, log.name))
    
    # Артефакты drunned (опционально - спрашиваем)
    if DRUNNED.exists():
        print("  Папка drunned/ содержит собранные артефакты.")
        print("  Очистить её? (y/N): ", end="", flush=True)
        response = input().strip().lower()
        if response in ['y', 'yes', 'д', 'да']:
            items_to_clean.append((DRUNNED, "drunned/"))
    
    # Разное
    misc = [
        ROOT / "liblib_simple.rlib",
    ]
    for item in misc:
        if item.exists():
            items_to_clean.append((item, item.name))
    
    if not items_to_clean:
        print("  Нечего чистить - репозиторий уже чистый!")
        return
    
    print(f"  Найдено {len(items_to_clean)} элементов для удаления:\n")
    
    total_size = 0
    for path, label in items_to_clean:
        if path.is_dir():
            size = sum(f.stat().st_size for f in path.rglob('*') if f.is_file())
        else:
            size = path.stat().st_size
        total_size += size
        size_mb = size / (1024 * 1024)
        print(f"    • {label:<40} ({size_mb:>8.2f} MB)")
    
    print(f"\n  Общий размер: {total_size / (1024 * 1024):.2f} MB")
    print(f"  Удалить всё? (y/N): ", end="", flush=True)
    
    response = input().strip().lower()
    if response not in ['y', 'yes', 'д', 'да']:
        print("\n  [ОТМЕНЕНО] Очистка отменена.")
        return
    
    print("\n  Удаление...")
    removed = 0
    for path, label in items_to_clean:
        try:
            if path.is_dir():
                shutil.rmtree(path)
                print(f"    ✓ {label}")
            else:
                path.unlink()
                print(f"    ✓ {label}")
            removed += 1
        except Exception as e:
            print(f"    ✗ {label} - {e}")
    
    print(f"\n[OK] Удалено {removed}/{len(items_to_clean)} элементов.")
    print(f"[OK] Освобождено ~{total_size / (1024 * 1024):.2f} MB.")
    print(f"\n  Директория очищена!")


# ── Точка входа ───────────────────────────────────────────────────────────────
def main():
    parser = argparse.ArgumentParser(
        prog="drun",
        description="dRun — тулчейн сборки PINDOS"
    )
    parser.add_argument("-b", action="store_true", help="собрать debug сборку")
    parser.add_argument("-c", action="store_true", help="проверить код на ошибки")
    parser.add_argument("-T", action="store_true", help="собрать и запустить QEMU")
    parser.add_argument("-r", metavar="VERSION",   help="release ISO (например: 0.1-moorino)")
    parser.add_argument("-cl", "--clean", action="store_true", help="очистить артефакты сборки")
    parser.add_argument("-get", action="store_true", help="установить все зависимости для сборки")
    
    args = parser.parse_args()
    
    if not any([args.b, args.c, args.T, args.r, args.clean, args.get]):
        parser.print_help()
        sys.exit(0)
    
    if args.get:
        cmd_get()
    elif args.c:
        cmd_check()
    elif args.b:
        cmd_build()
    elif args.T:
        cmd_test()
    elif args.r:
        cmd_release(args.r)
    elif args.clean:
        cmd_clean()

if __name__ == "__main__":
    main()
