@echo off
echo Building ISO image...

REM Clean up
if exist pagh.iso del pagh.iso

REM Create ISO using xorriso (if available) or fall back to manual method
where xorriso >nul 2>&1
if %errorlevel% equ 0 (
    echo Using xorriso...
    xorriso -as mkisofs -b BOOTX64.EFI -no-emul-boot -boot-load-size 4 -boot-info-table --efi-boot BOOTX64.EFI -o pagh.iso iso_root
) else (
    echo xorriso not found, trying oscdimg...
    where oscdimg >nul 2>&1
    if %errorlevel% equ 0 (
        oscdimg -u2 -udfver102 iso_root pagh.iso
    ) else (
        echo ERROR: No ISO creation tool found!
        echo Please install xorriso or use Windows ADK oscdimg
        exit /b 1
    )
)

if exist pagh.iso (
    echo ISO created successfully!
    dir pagh.iso
) else (
    echo ERROR: ISO creation failed!
    exit /b 1
)
