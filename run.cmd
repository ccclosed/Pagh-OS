@echo off
REM ============================================================
REM  pagh OS -- build, link, and run in QEMU (Limine, UEFI mode)
REM ------------------------------------------------------------
REM  Usage:
REM    run.cmd                 build + link + boot (debug)
REM    run.cmd run             same as above (explicit)
REM    run.cmd build           build + link only (no QEMU)
REM    run.cmd run release     boot a release build
REM    run.cmd build release   build + link a release build only
REM    run.cmd release         shorthand for `run release`
REM ============================================================
setlocal enabledelayedexpansion

REM --- Tunables -------------------------------------------------
set KERNEL=PAGH
set TARGET=x86_64-unknown-none
set LIMINE_DIR=limine-12.3.1
set OVMF=OVMF.fd
set DISK=disk.img

REM --- Argument parsing ----------------------------------------
REM First arg is the mode (run|build), second is the build type
REM (debug|release). As a convenience, `run.cmd release` / `debug`
REM treats the first arg as the build type and defaults to `run`.
set MODE=%1
set BUILD_TYPE=%2

if "%MODE%"=="" set MODE=run
if /i "%MODE%"=="release" set BUILD_TYPE=release
if /i "%MODE%"=="debug"   set BUILD_TYPE=debug
if /i "%MODE%"=="release" set MODE=run
if /i "%MODE%"=="debug"   set MODE=run
if "%BUILD_TYPE%"=="" set BUILD_TYPE=debug

if /i "%MODE%"=="build" goto :build
if /i "%MODE%"=="run"   goto :build
goto :usage

:build
echo === Building %KERNEL% (%BUILD_TYPE%) ===
if /i "%BUILD_TYPE%"=="release" (cargo build --release) else (cargo build)
if errorlevel 1 (echo BUILD FAILED & exit /b 1)

REM Select the static library and output ELF for the chosen profile.
if /i "%BUILD_TYPE%"=="release" (
    set KERNEL_ARCHIVE=target\%TARGET%\release\lib%KERNEL%.a
    set KERNEL_BIN=target\%TARGET%\release\%KERNEL%.elf
) else (
    set KERNEL_ARCHIVE=target\%TARGET%\debug\lib%KERNEL%.a
    set KERNEL_BIN=target\%TARGET%\debug\%KERNEL%.elf
)

if not exist "!KERNEL_ARCHIVE!" (
    echo ERROR: static library not found: !KERNEL_ARCHIVE!
    exit /b 1
)

REM --- Locate rust-lld in the nightly toolchain ----------------
REM cargo emits a staticlib; we link it ourselves with the LLVM
REM linker shipped by the nightly toolchain (excluding lldb).
set RUSTLLD=
for /f "delims=" %%p in ('where /R "%USERPROFILE%\.rustup" rust-lld.exe 2^>nul ^| findstr /i "nightly" ^| findstr /v "lldb"') do set RUSTLLD=%%p
if "!RUSTLLD!"=="" (
    echo ERROR: rust-lld.exe not found in the .rustup nightly toolchain
    exit /b 1
)

REM Whole-archive link so no kernel section is dropped as "unused";
REM linker.ld places the higher-half image at 0xffffffff80000000.
echo === Linking !KERNEL_BIN! ===
"!RUSTLLD!" -flavor gnu -T linker.ld -nostdlib -static --whole-archive "!KERNEL_ARCHIVE!" --no-whole-archive -o "!KERNEL_BIN!"
if errorlevel 1 (
    echo LINK FAILED
    exit /b 1
)
echo Link OK: !KERNEL_BIN!

if /i "%MODE%"=="build" goto :done

:run_qemu
REM --- Stage the Limine ESP (iso_root) -------------------------
echo === Preparing Limine boot directory ===

REM Wipe the staging dir first so neither QEMU's FAT layer nor
REM Windows can serve a stale kernel from a previous run.
if exist iso_root rmdir /S /Q iso_root

REM Recreate the EFI System Partition tree (mkdir makes parents).
mkdir iso_root\EFI\BOOT

REM Copy the freshly linked kernel under the name limine.conf expects.
copy /Y "!KERNEL_BIN!" iso_root\pagh.elf >nul

REM Copy the Limine UEFI loader; a missing loader is fatal (no boot).
if exist "%LIMINE_DIR%\BOOTX64.EFI" (
    copy /Y "%LIMINE_DIR%\BOOTX64.EFI" iso_root\EFI\BOOT\ >nul
) else (
    goto :err_no_loader
)

REM Generate limine.conf on the fly (Limine 12.x format).
REM '>' creates/overwrites the file; '>>' appends each subsequent line.
echo timeout: 5> iso_root\limine.conf
echo verbose: yes>> iso_root\limine.conf
echo serial: yes>> iso_root\limine.conf
echo.>> iso_root\limine.conf
echo /pagh OS>> iso_root\limine.conf
echo     protocol: limine>> iso_root\limine.conf
echo     kernel_path: boot():/pagh.elf>> iso_root\limine.conf

REM Also drop the config beside the loader so Limine always finds it.
copy /Y iso_root\limine.conf iso_root\EFI\BOOT\limine.conf >nul

REM --- Create the virtio-blk disk image (64 MiB raw) on demand --
if not exist %DISK% (
    echo === Creating %DISK% ^(64 MiB raw^) ===
    where qemu-img >nul 2>&1
    if !errorlevel! equ 0 (
        qemu-img create -f raw %DISK% 64M
    ) else (
        echo qemu-img not found, falling back to fsutil
        fsutil file createnew %DISK% 67108864
    )
)

REM --- Preflight: required host tools and firmware --------------
if not exist %OVMF% goto :err_no_bios

where qemu-system-x86_64 >nul 2>&1
if errorlevel 1 (
    echo ERROR: qemu-system-x86_64 not found in PATH.
    exit /b 1
)

echo === Starting QEMU (UEFI) ===
echo Serial output on this console. Press Ctrl+A then X to exit.
echo.

REM QEMU flags:
REM   -bios OVMF.fd ............ UEFI firmware
REM   -drive fat:rw:iso_root ... expose iso_root as a virtual FAT ESP
REM   virtio-blk-pci (hd0) ..... the ext2 data disk (disk.img) at /mnt
REM   -netdev user + virtio-net  user-mode NIC, host:5555 -> guest:7 (TCP/UDP)
REM   -m 512M .................. guest RAM
REM   -serial stdio ............ kernel serial on this console
REM   -no-reboot/-no-shutdown .. freeze on triple fault instead of looping
REM   -d ... -D qemu_debug.log . interrupt/reset/guest-error trace to a file
qemu-system-x86_64 ^
    -bios %OVMF% ^
    -drive file=fat:rw:iso_root,format=raw ^
    -drive file=%DISK%,format=raw,if=none,id=hd0 ^
    -device virtio-blk-pci,drive=hd0 ^
    -netdev user,id=net0,hostfwd=tcp::5555-:7,hostfwd=udp::5555-:7 ^
    -device virtio-net-pci,netdev=net0 ^
    -m 512M ^
    -serial stdio ^
    -no-reboot ^
    -no-shutdown ^
    -d int,cpu_reset,guest_errors ^
    -D qemu_debug.log

goto :done

REM --- Error exits ---------------------------------------------
:err_no_loader
echo ERROR: Limine loader not found: %LIMINE_DIR%\BOOTX64.EFI
echo Download the Limine %LIMINE_DIR% tree and place BOOTX64.EFI inside it.
exit /b 1

:err_no_bios
echo ERROR: %OVMF% not found in the project root.
echo Download the OVMF UEFI firmware and place it next to this script.
exit /b 1

:usage
echo === pagh OS run script ===
echo.
echo Usage: run.cmd [mode] [build_type]
echo   mode:        run ^(default^) ^| build
echo   build_type:  debug ^(default^) ^| release
echo.
echo Examples:
echo   run.cmd                 build + link + boot ^(debug^)
echo   run.cmd build           build + link only
echo   run.cmd run release     boot a release build
echo   run.cmd release         shorthand for `run release`
echo.

:done
endlocal
echo Done.
