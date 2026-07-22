@echo off
REM Background QEMU launcher for Task 3 (virtio-blk) boot-smoke verification.
REM Mirrors run.cmd's device set (virtio-blk + virtio-net) but writes serial to
REM serial.log and uses timeout 0 so it can run unattended in the background.
setlocal
set KERNEL_BIN=target\x86_64-unknown-none\debug\PAGH.elf

if exist iso_root rmdir /S /Q iso_root
mkdir iso_root
mkdir iso_root\EFI\BOOT
copy /Y "%KERNEL_BIN%" iso_root\pagh.elf >nul
copy /Y "limine-12.3.1\BOOTX64.EFI" iso_root\EFI\BOOT\ >nul

echo timeout: 0> iso_root\limine.conf
echo verbose: yes>> iso_root\limine.conf
echo serial: yes>> iso_root\limine.conf
echo.>> iso_root\limine.conf
echo /pagh OS>> iso_root\limine.conf
echo     protocol: limine>> iso_root\limine.conf
echo     kernel_path: boot():/pagh.elf>> iso_root\limine.conf
copy /Y iso_root\limine.conf iso_root\EFI\BOOT\limine.conf >nul

if not exist disk.img (
    qemu-img create -f raw disk.img 64M
)

qemu-system-x86_64 ^
    -bios OVMF.fd ^
    -drive file=fat:rw:iso_root,format=raw ^
    -drive file=disk.img,format=raw,if=none,id=hd0 ^
    -device virtio-blk-pci,drive=hd0 ^
    -netdev user,id=net0,hostfwd=tcp::5555-:7,hostfwd=udp::5555-:7 ^
    -device virtio-net-pci,netdev=net0 ^
    -m 512M ^
    -serial file:serial.log ^
    -no-reboot ^
    -no-shutdown ^
    -d int,cpu_reset,guest_errors ^
    -D qemu_debug.log
endlocal
