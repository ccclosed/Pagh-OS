@echo off
REM ============================================================
REM  pagh OS -- build, link & run in QEMU (Limine UEFI Mode)
REM ============================================================
setlocal enabledelayedexpansion

set KERNEL=PAGH
set TARGET=x86_64-unknown-none
set BUILD_TYPE=%2
if "%BUILD_TYPE%"=="" set BUILD_TYPE=debug

set MODE=%1
if "%MODE%"=="" set MODE=run

if "%MODE%"=="build" goto :build
if "%MODE%"=="run"   goto :build
goto :usage

:build
echo === Building %KERNEL% (%BUILD_TYPE%) ===
if "%BUILD_TYPE%"=="release" (cargo build --release) else (cargo build)
if errorlevel 1 (echo BUILD FAILED & exit /b 1)

if "%BUILD_TYPE%"=="release" (
    set KERNEL_ARCHIVE=target\%TARGET%\release\lib%KERNEL%.a
    set KERNEL_BIN=target\%TARGET%\release\%KERNEL%.elf
) else (
    set KERNEL_ARCHIVE=target\%TARGET%\debug\lib%KERNEL%.a
    set KERNEL_BIN=target\%TARGET%\debug\%KERNEL%.elf
)

if not exist "!KERNEL_ARCHIVE!" (
    echo ERROR: Static library not found: !KERNEL_ARCHIVE!
    exit /b 1
)

REM --- Find rust-lld ---
set RUSTLLD=
for /f "delims=" %%p in ('where /R "%USERPROFILE%\.rustup" rust-lld.exe 2^>nul ^| findstr /i "nightly" ^| findstr /v "lldb"') do set RUSTLLD=%%p
if "!RUSTLLD!"=="" (
    echo ERROR: rust-lld.exe not found in .rustup nightly toolchain
    exit /b 1
)

echo === Linking !KERNEL_BIN! ===
"!RUSTLLD!" -flavor gnu -T linker.ld -nostdlib -static --whole-archive "!KERNEL_ARCHIVE!" --no-whole-archive -o "!KERNEL_BIN!"
if errorlevel 1 (
    echo LINK FAILED
    exit /b 1
)
echo Link OK: !KERNEL_BIN!

if "%MODE%"=="build" goto :done

:run_qemu
echo === Preparing Limine Directory ===
:: Жестко сносим старую папку, чтобы очистить кэш QEMU и Windows
if exist iso_root rmdir /S /Q iso_root

:: Создаем структуру заново
mkdir iso_root
mkdir iso_root\EFI\BOOT

:: Копируем свежескомпилированное ядро
copy /Y "!KERNEL_BIN!" iso_root\pagh.elf >nul

:: Копируем Limine BOOTX64.EFI из скачанной папки
if exist "limine-12.3.1\BOOTX64.EFI" (
    copy /Y "limine-12.3.1\BOOTX64.EFI" iso_root\EFI\BOOT\ >nul
) else (
    echo WARNING: BOOTX64.EFI not found in limine-12.3.1 folder
)

:: ГЕНЕРИРУЕМ КОНФИГ НА ЛЕТУ (формат Limine 12.x)
:: Символ > перезаписывает файл, >> добавляет строки.
echo timeout: 5> iso_root\limine.conf
echo verbose: yes>> iso_root\limine.conf
echo serial: yes>> iso_root\limine.conf
echo.>> iso_root\limine.conf
echo /pagh OS>> iso_root\limine.conf
echo     protocol: limine>> iso_root\limine.conf
echo     kernel_path: boot():/pagh.elf>> iso_root\limine.conf

:: Дублируем конфиг в папку к загрузчику, чтобы у Limine вообще не было шансов его не найти
copy /Y iso_root\limine.conf iso_root\EFI\BOOT\limine.conf >nul

:: --- Create the virtio-blk disk image (64 MiB raw) if it doesn't exist ---
if not exist disk.img (
    echo === Creating disk.img ^(64 MiB raw^) ===
    where qemu-img >nul 2>&1
    if !errorlevel! equ 0 (
        qemu-img create -f raw disk.img 64M
    ) else (
        echo qemu-img not found, falling back to fsutil
        fsutil file createnew disk.img 67108864
    )
)

echo === Starting QEMU via UEFI ===
echo Serial output on console. Ctrl+A X to exit.
echo.

where qemu-system-x86_64 >nul 2>&1
if %errorlevel% neq 0 (
    echo ERROR: qemu-system-x86_64 not found in PATH.
    exit /b 1
)

:: Запуск QEMU с эмуляцией папки iso_root как FAT-диска
qemu-system-x86_64 ^
    -bios OVMF.fd ^
    -drive file=fat:rw:iso_root,format=raw ^
    -drive file=disk.img,format=raw,if=none,id=hd0 ^
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

:err_no_loader
echo ERROR: Missing Limine loader BOOTX64.EFI
echo Please put it into iso_root\EFI\BOOT\
exit /b 1

:err_no_conf
echo ERROR: limine.conf missing in iso_root\
exit /b 1

:err_no_bios
echo ERROR: OVMF.fd not found in project root!
echo Please download OVMF.fd and place it next to this script.
exit /b 1

:usage
echo === pagh OS run script ===
echo.
echo Usage: run.cmd [mode] [build_type]
echo   run     - Build + Link + Run QEMU (default)
echo   build   - Build + Link only
echo   debug   - Debug build (default)
echo   release - Release build
echo.

:done
endlocal
echo Done.