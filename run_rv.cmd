@echo off
REM ============================================================
REM  pagh OS -- build & boot the riscv64 kernel (QEMU virt + OpenSBI)
REM ------------------------------------------------------------
REM  Branch `riscv-port`. Builds the MAIN crate for riscv64gc (the riscv code
REM  lives in src/arch/riscv64/, gated by cfg(target_arch="riscv64")), links the
REM  staticlib with rust-lld + linker-riscv.ld, and boots it in S-mode under
REM  QEMU's built-in OpenSBI, with a virtio-blk disk and a virtio-net NIC.
REM
REM  Usage:
REM    run_rv.cmd          build + link + boot
REM    run_rv.cmd build    build + link only
REM ============================================================
setlocal enabledelayedexpansion
set TARGET=riscv64gc-unknown-none-elf
set ARCHIVE=target\%TARGET%\debug\libpagh.a
set ELF=PAGH-rv.elf
set DISK=rvdisk.img
set MODE=%1
if "%MODE%"=="" set MODE=run

echo === Building pagh (riscv64) ===
cargo +nightly build --target %TARGET%
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
