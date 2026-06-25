@echo off
REM ============================================================
REM  pagh OS -- build & boot the riscv64 seed (QEMU virt + OpenSBI)
REM ------------------------------------------------------------
REM  Branch `riscv-port`. Builds the standalone seed crate in rv/
REM  for riscv64gc (build-std) and boots it in S-mode under QEMU's
REM  built-in OpenSBI, with a virtio-blk disk on virtio-mmio.
REM  Serial is on the console.
REM
REM  Usage:
REM    run_rv.cmd          build + boot
REM    run_rv.cmd build    build only
REM ============================================================
setlocal
set ELF=rv\target\riscv64gc-unknown-none-elf\release\rv
set DISK=rv\rvdisk.img
set MODE=%1
if "%MODE%"=="" set MODE=run

echo === Building riscv64 seed (release) ===
pushd rv
cargo +nightly build --release
set ERR=%errorlevel%
popd
if not "%ERR%"=="0" (echo BUILD FAILED & exit /b 1)
echo Build OK: %ELF%

if /i "%MODE%"=="build" goto :done

where qemu-system-riscv64 >nul 2>&1
if errorlevel 1 (
    echo ERROR: qemu-system-riscv64 not found in PATH.
    exit /b 1
)

REM Create a 16 MiB scratch virtio-blk disk image on first run.
if not exist %DISK% (
    echo === Creating %DISK% ^(16 MiB raw^) ===
    where qemu-img >nul 2>&1
    if errorlevel 1 (
        fsutil file createnew %DISK% 16777216
    ) else (
        qemu-img create -f raw %DISK% 16M
    )
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
