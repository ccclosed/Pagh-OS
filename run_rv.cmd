@echo off
REM ============================================================
REM  pagh OS -- build & boot the riscv64 seed (QEMU virt + OpenSBI)
REM ------------------------------------------------------------
REM  Branch `riscv-port`, Milestone A. Builds the standalone seed
REM  crate in rv/ for riscv64gc (build-std) and boots it in S-mode
REM  under QEMU's built-in OpenSBI. Serial is on the console.
REM
REM  Usage:
REM    run_rv.cmd          build + boot
REM    run_rv.cmd build    build only
REM ============================================================
setlocal
set ELF=rv\target\riscv64gc-unknown-none-elf\release\rv
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

echo === Booting QEMU (riscv64 virt, OpenSBI, S-mode) ===
echo Serial output on this console. Press Ctrl+A then X to exit.
echo.
qemu-system-riscv64 -machine virt -nographic -bios default -kernel %ELF%

:done
endlocal
echo Done.
