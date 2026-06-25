@echo off
REM ============================================================
REM  pagh OS (riscv-port) -- build & boot the riscv64 kernel
REM ------------------------------------------------------------
REM  This branch is a standalone riscv64 kernel (the x86_64 kernel lives on the
REM  `main` branch). `cargo build` targets riscv64gc-unknown-none-elf via
REM  .cargo/config.toml; we link the staticlib with rust-lld + linker-riscv.ld
REM  and boot it in S-mode under QEMU's built-in OpenSBI, with a virtio-blk disk
REM  and a virtio-net NIC on the virtio-mmio transport. Serial is on the console.
REM
REM  Usage:
REM    run.cmd          build + link + boot
REM    run.cmd build    build + link only
REM ============================================================
setlocal enabledelayedexpansion
set TARGET=riscv64gc-unknown-none-elf
set ARCHIVE=target\%TARGET%\debug\libpagh.a
set ELF=PAGH.elf
set DISK=rvdisk.img
set MODE=%1
if "%MODE%"=="" set MODE=run

echo === Building pagh (riscv64) ===
cargo +nightly build
if errorlevel 1 (echo BUILD FAILED & exit /b 1)
if not exist "%ARCHIVE%" (echo ERROR: static library not found: %ARCHIVE% & exit /b 1)

REM --- Locate rust-lld in the nightly toolchain ---
set RUSTLLD=
for /f "delims=" %%p in ('where /R "%USERPROFILE%\.rustup" rust-lld.exe 2^>nul ^| findstr /i "nightly" ^| findstr /v "lldb"') do set RUSTLLD=%%p
if "!RUSTLLD!"=="" (echo ERROR: rust-lld.exe not found in the .rustup nightly toolchain & exit /b 1)

echo === Linking %ELF% ===
"!RUSTLLD!" -flavor gnu -T linker-riscv.ld -nostdlib -static --whole-archive "%ARCHIVE%" --no-whole-archive -o "%ELF%"
if errorlevel 1 (echo LINK FAILED & exit /b 1)
echo Link OK: %ELF%

if /i "%MODE%"=="build" goto :done

where qemu-system-riscv64 >nul 2>&1
if errorlevel 1 (echo ERROR: qemu-system-riscv64 not found in PATH. & exit /b 1)

if not exist %DISK% (
    echo === Creating %DISK% ^(16 MiB raw^) ===
    where qemu-img >nul 2>&1
    if errorlevel 1 (fsutil file createnew %DISK% 16777216) else (qemu-img create -f raw %DISK% 16M)
)

echo === Booting QEMU (riscv64 virt, OpenSBI, S-mode) ===
echo Serial output on this console. Press Ctrl+A then X to exit.
echo.
qemu-system-riscv64 -machine virt -m 256M -nographic -bios default ^
    -kernel %ELF% ^
    -drive file=%DISK%,format=raw,if=none,id=hd0 ^
    -device virtio-blk-device,drive=hd0 ^
    -netdev user,id=net0 ^
    -device virtio-net-device,netdev=net0

:done
endlocal
echo Done.
