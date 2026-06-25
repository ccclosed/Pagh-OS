<#
.SYNOPSIS
  Deterministic local-mirror apt end-to-end test for Pagh-OS (spec task 10.1).

.DESCRIPTION
  Repeatable harness that proves the full by-name `apt` pipeline against a local
  HTTP Debian-style mirror, with NO network dependency and NO manual QA.

  It reuses the project's existing automation hook -- the `lx_selftest` cargo
  feature. When the kernel is built with that feature, boot::kernel_main spawns
  selftest_lx::run_post_net_checks() which calls run_apt_e2e():
      apt setmirror http://10.0.2.2:8000 /
      apt update                 (gz-first index fetch + stream parse)
      apt install hello-pagh      (resolve -> fetch .deb -> data.tar -> ext2 /mnt)
      run /mnt/usr/bin/hello-pagh (loads + runs the installed static Linux ELF)
  and tools/mini_repo.py serves the sized Packages.gz + the referenced .deb on
  0.0.0.0:8000 (the QEMU user-net host gateway 10.0.2.2 reaches it from the guest).

  Steps:
    1. cargo build --release --features lx_selftest, link the release PAGH.elf.
    2. Stage iso_root (pagh.elf + Limine loader + limine.conf).
    3. Start the local mirror (python tools/mini_repo.py serve 8000).
    4. Boot the release ELF under QEMU, serial -> log file.
    5. Poll the serial log and assert the e2e markers.
    6. Tear down QEMU + mirror.
    7. Restore the DEFAULT (no-feature) release ELF into iso_root\pagh.elf.

.PARAMETER Port
  Mirror HTTP port (default 8000).
.PARAMETER TimeoutSec
  Max seconds to wait for the e2e result on serial (default 120).
.PARAMETER KeepArtifacts
  Keep the serial log and feature ELF instead of leaving only the default ELF.
#>
[CmdletBinding()]
param(
    [int]$Port = 8000,
    [int]$TimeoutSec = 120,
    [switch]$KeepArtifacts
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot   # workspace root (parent of tools\)
Set-Location $root

$TARGET   = 'x86_64-unknown-none'
$relArchive = "target\$TARGET\release\libPAGH.a"
$relElf     = "target\$TARGET\release\PAGH.elf"
$serialLog  = Join-Path $root 'serial_e2e.log'
$qemuLog    = Join-Path $root 'qemu_e2e_debug.log'

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
    Write-Host "Link OK: $relElf"
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

# ---------------------------------------------------------------------------
# 1. Build (feature) + link
# ---------------------------------------------------------------------------
$lld = Find-RustLld
Write-Host '=== Building PAGH (release, --features lx_selftest) ===' -ForegroundColor Cyan
cargo build --release --features lx_selftest
if (-not (Test-Path $relArchive)) { throw "build failed: $relArchive missing" }
Link-Kernel $lld
Stage-IsoRoot
Ensure-Disk

# Make sure the mirror tree is freshly built and learn what it serves.
Write-Host '=== Building mini_repo tree ===' -ForegroundColor Cyan
python tools\mini_repo.py build

# ---------------------------------------------------------------------------
# 2. Start the local mirror
# ---------------------------------------------------------------------------
if (Test-Path $serialLog) { Remove-Item $serialLog -Force }
Write-Host "=== Starting local mirror on 0.0.0.0:$Port ===" -ForegroundColor Cyan
$mirror = Start-Process -FilePath 'python' -ArgumentList "tools\mini_repo.py","serve","$Port" `
    -PassThru -WindowStyle Hidden -RedirectStandardOutput mirror_out.log -RedirectStandardError mirror_err.log
Start-Sleep -Seconds 2

# ---------------------------------------------------------------------------
# 3. Boot the release ELF under QEMU (serial -> file)
# ---------------------------------------------------------------------------
Write-Host '=== Booting release ELF under QEMU ===' -ForegroundColor Cyan
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

# ---------------------------------------------------------------------------
# 4. Poll the serial log for the e2e result
# ---------------------------------------------------------------------------
Write-Host "=== Waiting up to $TimeoutSec s for e2e result on serial ===" -ForegroundColor Cyan
$deadline = (Get-Date).AddSeconds($TimeoutSec)
$done = $false
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 3
    if (Test-Path $serialLog) {
        $txt = Get-Content $serialLog -Raw -ErrorAction SilentlyContinue
        if ($txt -match 'LXSELFTEST apt_e2e PASS' -or $txt -match 'LXSELFTEST apt_e2e FAIL') {
            # give the scheduler a moment to also emit hello-from-apt
            Start-Sleep -Seconds 4
            $done = $true
            break
        }
    }
}

# ---------------------------------------------------------------------------
# 5. Tear down
# ---------------------------------------------------------------------------
foreach ($p in @($qemu,$mirror)) {
    if ($p -and -not $p.HasExited) { try { $p.Kill() } catch {} }
}

# ---------------------------------------------------------------------------
# 6. Assertions over the captured serial log
# ---------------------------------------------------------------------------
$serial = if (Test-Path $serialLog) { Get-Content $serialLog -Raw } else { '' }
Write-Host "`n================ SERIAL EVIDENCE ================" -ForegroundColor Yellow
$serial -split "`r?`n" | Select-String -Pattern 'apt:|LXSELFTEST apt_e2e|hello from apt' | ForEach-Object { $_.Line }

function Test-Assert($label, $regex) {
    $m = [regex]::Match($serial, $regex)
    if ($m.Success) { Write-Host ("PASS  {0}: {1}" -f $label, $m.Value.Trim()) -ForegroundColor Green; return $true }
    Write-Host ("FAIL  {0}" -f $label) -ForegroundColor Red; return $false
}

Write-Host "`n================ ASSERTIONS ================" -ForegroundColor Yellow
$a1 = Test-Assert 'index loaded (>0 packages)' 'apt: index loaded \((\d+) packages\)'
# NOTE: the kernel logs the mirror host without the port (scheme://host + path),
# so the first index fetch line reads http://10.0.2.2/.../Packages.gz (no :8000).
$a2 = Test-Assert 'gz-first index fetch (R5.2)' 'apt: fetching index http://10\.0\.2\.2\S*Packages\.gz'
$a3 = Test-Assert 'install wrote files onto ext2 /mnt' 'apt: installed \S+ \(\d+ files\)'
$a4 = Test-Assert 'apt_e2e PASS' 'LXSELFTEST apt_e2e PASS[^\r\n]*'
$a5 = Test-Assert 'installed static binary ran' 'hello from apt'

$ok = $a1 -and $a2 -and $a3 -and $a4 -and $a5

# ---------------------------------------------------------------------------
# 7. Restore the DEFAULT (no-feature) release ELF into iso_root\pagh.elf
# ---------------------------------------------------------------------------
Write-Host "`n=== Restoring default (no-feature) release ELF ===" -ForegroundColor Cyan
cargo build --release
Link-Kernel $lld
Copy-Item $relElf iso_root\pagh.elf -Force
Write-Host 'Default release ELF restored into iso_root\pagh.elf'

if (-not $KeepArtifacts) {
    Remove-Item mirror_out.log,mirror_err.log -ErrorAction SilentlyContinue
}

Write-Host ""
if ($ok) { Write-Host 'E2E RESULT: PASS' -ForegroundColor Green; exit 0 }
else     { Write-Host 'E2E RESULT: FAIL (see serial evidence above)' -ForegroundColor Red; exit 1 }
