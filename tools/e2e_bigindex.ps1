<#
.SYNOPSIS
  DIAGNOSTIC (Part B): reproduce + discriminate the `apt update` parse-stage
  crash (#14 PF, RIP=0x1) over a LOCAL big index. Clone of e2e_local_mirror.ps1.

.DESCRIPTION
  Repeatable, NO-live-CDN harness that drives the kernel's `lx_bigindex` boot
  self-test against a large synthetic Debian-style mirror served locally by
  `tools/mini_repo.py bigindex N 8000`.

  It mirrors e2e_local_mirror.ps1 but:
    * builds release with `--features lx_bigindex` (add `-InRam` to also enable
      `lx_bigindex_inram`, the "Test A" in-RAM / no-concurrent-net variant),
    * serves the BIG index (`bigindex N`) instead of the tiny hello-pagh repo,
    * scans serial for the parse-stage crash marker `[EXC #14] ... RIP=0x...01`
      and the `BIGINDEX ...` / `apt: ... parsed P packages` progress evidence.

  Steps:
    1. cargo build --release [--features lx_bigindex[,lx_bigindex_inram]], link PAGH.elf.
    2. Stage iso_root (pagh.elf + Limine loader + limine.conf).
    3. Start the local BIG-index mirror (python tools/mini_repo.py bigindex N port).
    4. Boot the feature ELF under QEMU, serial -> log file.
    5. Poll the serial log for a crash marker OR a PASS/terminal line.
    6. Tear down QEMU + mirror.
    7. Restore the DEFAULT (no-feature) release ELF into iso_root\pagh.elf.

.PARAMETER Stanzas    Number of synthetic packages to generate/serve (default 60000).
.PARAMETER Port       Mirror HTTP port (default 8000).
.PARAMETER TimeoutSec Max seconds to wait on serial (default 240).
.PARAMETER InRam      Also enable `lx_bigindex_inram` (Test A: in-RAM, no concurrent net).
.PARAMETER KeepArtifacts  Keep the serial log + feature ELF instead of only the default ELF.
#>
[CmdletBinding()]
param(
    [int]$Stanzas = 60000,
    [int]$Port = 8000,
    [int]$TimeoutSec = 240,
    [switch]$InRam,
    [switch]$KeepArtifacts
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$TARGET   = 'x86_64-unknown-none'
$relArchive = "target\$TARGET\release\libPAGH.a"
$relElf     = "target\$TARGET\release\PAGH.elf"
$serialLog  = Join-Path $root 'serial_bigindex.log'
$qemuLog    = Join-Path $root 'qemu_bigindex_debug.log'

$features = 'lx_bigindex'
if ($InRam) { $features = 'lx_bigindex,lx_bigindex_inram' }

function Find-RustLld {
    $hits = Get-ChildItem -Path "$env:USERPROFILE\.rustup" -Recurse -Filter 'rust-lld.exe' -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match 'nightly' -and $_.FullName -notmatch 'lldb' }
    if (-not $hits) { throw 'rust-lld.exe not found in .rustup nightly toolchain' }
    return $hits[0].FullName
}

function Link-Kernel($lld) {
    Write-Host "=== Linking $relElf ===" -ForegroundColor Cyan
    & $lld -flavor gnu -T linker.ld -nostdlib -static --whole-archive $relArchive --no-whole-archive -o $relElf
    if (-not (Test-Path $relElf)) { throw "link failed: $relElf not produced" }
}

function Stage-IsoRoot {
    if (Test-Path iso_root) { Remove-Item -Recurse -Force iso_root }
    New-Item -ItemType Directory -Path iso_root\EFI\BOOT | Out-Null
    Copy-Item $relElf iso_root\pagh.elf -Force
    $loader = 'limine-12.3.1\BOOTX64.EFI'
    if (Test-Path $loader) { Copy-Item $loader iso_root\EFI\BOOT\ -Force }
    else { throw "Limine loader missing: $loader" }
    $conf = "timeout: 5`r`nverbose: yes`r`nserial: yes`r`n`r`n/pagh OS`r`n    protocol: limine`r`n    kernel_path: boot():/pagh.elf`r`n"
    Set-Content -Path iso_root\limine.conf -Value $conf -NoNewline
    Copy-Item iso_root\limine.conf iso_root\EFI\BOOT\limine.conf -Force
}

function Ensure-Disk {
    if (-not (Test-Path disk.img)) {
        if (Get-Command qemu-img -ErrorAction SilentlyContinue) { qemu-img create -f raw disk.img 64M | Out-Null }
        else { fsutil file createnew disk.img 67108864 | Out-Null }
    }
}

# 1. Build (feature) + link
$lld = Find-RustLld
Write-Host "=== Building PAGH (release, --features $features) ===" -ForegroundColor Cyan
cargo build --release --features $features
if (-not (Test-Path $relArchive)) { throw "build failed: $relArchive missing" }
Link-Kernel $lld
Stage-IsoRoot
Ensure-Disk

# 2. Start the local BIG-index mirror
if (Test-Path $serialLog) { Remove-Item $serialLog -Force }
Write-Host "=== Starting BIG-index mirror ($Stanzas stanzas) on 0.0.0.0:$Port ===" -ForegroundColor Cyan
$mirror = Start-Process -FilePath 'python' -ArgumentList "tools\mini_repo.py","bigindex","$Stanzas","$Port" `
    -PassThru -WindowStyle Hidden -RedirectStandardOutput mirror_out.log -RedirectStandardError mirror_err.log
Start-Sleep -Seconds 5   # give the generator time to build + start serving

# 3. Boot the feature ELF under QEMU (serial -> file)
Write-Host '=== Booting feature ELF under QEMU ===' -ForegroundColor Cyan
$qemuArgs = @(
    '-bios','OVMF.fd',
    '-drive','file=fat:rw:iso_root,format=raw',
    '-drive','file=disk.img,format=raw,if=none,id=hd0',
    '-device','virtio-blk-pci,drive=hd0',
    '-netdev','user,id=net0',
    '-device','virtio-net-pci,netdev=net0',
    '-m','512M',
    '-serial',"file:$serialLog",
    '-display','none',
    '-no-reboot',
    '-d','guest_errors',
    '-D',$qemuLog
)
$qemu = Start-Process -FilePath 'qemu-system-x86_64' -ArgumentList $qemuArgs -PassThru -WindowStyle Hidden

# 4. Poll the serial log
Write-Host "=== Waiting up to $TimeoutSec s for crash/terminal marker ===" -ForegroundColor Cyan
$deadline = (Get-Date).AddSeconds($TimeoutSec)
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 3
    if (Test-Path $serialLog) {
        $txt = Get-Content $serialLog -Raw -ErrorAction SilentlyContinue
        if ($txt -match 'EXC #14' -or $txt -match 'LXSELFTEST bigindex (PASS|FAIL)') {
            Start-Sleep -Seconds 2
            break
        }
    }
}

# 5. Tear down
foreach ($p in @($qemu,$mirror)) {
    if ($p -and -not $p.HasExited) { try { $p.Kill() } catch {} }
}

# 6. Evidence
$serial = if (Test-Path $serialLog) { Get-Content $serialLog -Raw } else { '' }
Write-Host "`n================ SERIAL EVIDENCE ================" -ForegroundColor Yellow
$serial -split "`r?`n" | Select-String -Pattern 'apt:|BIGINDEX|LXSELFTEST bigindex|EXC #14|PAGE FAULT|RIP=' | ForEach-Object { $_.Line }

$crash = [regex]::Match($serial, 'EXC #14[^\r\n]*')
if ($crash.Success) {
    Write-Host "`nCRASH REPRODUCED: $($crash.Value.Trim())" -ForegroundColor Red
} else {
    Write-Host "`nNo #14 crash marker observed in this run." -ForegroundColor Green
}

# 7. Restore the DEFAULT (no-feature) release ELF into iso_root\pagh.elf
Write-Host "`n=== Restoring default (no-feature) release ELF ===" -ForegroundColor Cyan
cargo build --release
Link-Kernel $lld
Copy-Item $relElf iso_root\pagh.elf -Force
Write-Host 'Default release ELF restored into iso_root\pagh.elf'

if (-not $KeepArtifacts) {
    Remove-Item mirror_out.log,mirror_err.log -ErrorAction SilentlyContinue
}
