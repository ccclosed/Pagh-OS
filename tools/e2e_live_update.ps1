<#
.SYNOPSIS
  Live full-`apt update` smoke against the real deb.debian.org mirror
  (spec full-debian-apt-update task 11.1).

.DESCRIPTION
  Boots a release Pagh-OS kernel built with the dedicated `lx_livetest` cargo
  feature, which (after DHCP comes up) runs the FULL live update pipeline against
  the DEFAULT mirror -- deb.debian.org /debian stable main amd64 over HTTPS
  (VARIANT A) -- with NO local mirror and the QEMU default user-net NAT providing
  outbound connectivity so the guest can reach the internet:

      apt::update()                         # stream-fetch + parse the real index
      LIVE_APT_UPDATE: count=N              # assert N >= 50000 (R1.2)
      Resident_Index_Footprint = ...        # R2.4/R6.2
      apt::install("busybox-static")        # R8.1-8.2
      run /mnt/bin/busybox via the loader   # R8.3

  This run is NETWORK-DEPENDENT and SLOW under QEMU/TCG. Per resolved open
  question Q-A the timing is SOFT and NON-BINDING: this harness does NOT gate on
  wall-clock. It gates on serial evidence. If the live download is still
  progressing at -TimeoutSec, it reports the LAST observed progress
  (decompressed KiB, parsed package count, whether progress was monotonically
  non-decreasing, and whether the terminal `index loaded` line was reached) --
  that honest partial evidence is an ACCEPTABLE outcome for this task.

  Steps (mirror tools/e2e_local_mirror.ps1, minus the local mirror):
    1. cargo build --release --features lx_livetest, link the release PAGH.elf.
    2. Stage iso_root (pagh.elf + Limine loader + limine.conf).
    3. Boot the release ELF under QEMU with outbound user-net, serial -> log file.
    4. Poll the serial log; report live-update progress / terminal / count.
    5. Tear down QEMU.
    6. Restore the DEFAULT (no-feature) release ELF into iso_root\pagh.elf.

.PARAMETER TimeoutSec
  Max seconds to wait for the live result on serial (default 1200). SOFT bound.
.PARAMETER KeepArtifacts
  Keep the serial log and feature ELF instead of leaving only the default ELF.
#>
[CmdletBinding()]
param(
    [int]$TimeoutSec = 1200,
    [switch]$KeepArtifacts
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot   # workspace root (parent of tools\)
Set-Location $root

$TARGET     = 'x86_64-unknown-none'
$relArchive = "target\$TARGET\release\libPAGH.a"
$relElf     = "target\$TARGET\release\PAGH.elf"
$serialLog  = Join-Path $root 'serial_live.log'
$qemuLog    = Join-Path $root 'qemu_live_debug.log'

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
# 1. Build (lx_livetest feature) + link
# ---------------------------------------------------------------------------
$lld = Find-RustLld
Write-Host '=== Building PAGH (release, --features lx_livetest) ===' -ForegroundColor Cyan
cargo build --release --features lx_livetest
if (-not (Test-Path $relArchive)) { throw "build failed: $relArchive missing" }
Link-Kernel $lld
Stage-IsoRoot
Ensure-Disk

# ---------------------------------------------------------------------------
# 2. Boot the release ELF under QEMU (default user-net = outbound NAT)
# ---------------------------------------------------------------------------
if (Test-Path $serialLog) { Remove-Item $serialLog -Force }
Write-Host '=== Booting release ELF under QEMU (outbound user-net to deb.debian.org) ===' -ForegroundColor Cyan
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
# 3. Poll the serial log for the live-update result (SOFT timeout)
# ---------------------------------------------------------------------------
Write-Host "=== Waiting up to $TimeoutSec s for live-update result on serial (SOFT, non-binding) ===" -ForegroundColor Cyan
$deadline = (Get-Date).AddSeconds($TimeoutSec)
$reachedTerminal = $false
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 5
    if (Test-Path $serialLog) {
        $txt = Get-Content $serialLog -Raw -ErrorAction SilentlyContinue
        if ($txt -match 'LXSELFTEST live_update PASS' -or $txt -match 'LXSELFTEST live_update FAIL') {
            Start-Sleep -Seconds 4   # let busybox output land
            $reachedTerminal = $true
            break
        }
        # Heartbeat: surface the most recent progress line as we wait.
        $last = ($txt -split "`r?`n" | Select-String -Pattern 'apt: decompressed .* parsed .* packages' | Select-Object -Last 1)
        if ($last) { Write-Host ("  ... {0}" -f $last.Line.Trim()) -ForegroundColor DarkGray }
    }
}

# ---------------------------------------------------------------------------
# 4. Tear down QEMU
# ---------------------------------------------------------------------------
if ($qemu -and -not $qemu.HasExited) { try { $qemu.Kill() } catch {} }

# ---------------------------------------------------------------------------
# 5. Parse + report the live evidence
# ---------------------------------------------------------------------------
$serial = if (Test-Path $serialLog) { Get-Content $serialLog -Raw } else { '' }

Write-Host "`n================ LIVE SERIAL EVIDENCE ================" -ForegroundColor Yellow
$serial -split "`r?`n" |
    Select-String -Pattern 'apt:|LIVE_APT_UPDATE|LXSELFTEST live_update|Resident_Index_Footprint|net::tls|busybox' |
    ForEach-Object { $_.Line }

# Progress lines: apt: decompressed <K> KiB, parsed <P> packages[...]
$progress = [regex]::Matches($serial, 'apt: decompressed (\d+) KiB, parsed (\d+) packages')
$kibSeq  = @(); $pkgSeq = @()
foreach ($m in $progress) { $kibSeq += [int64]$m.Groups[1].Value; $pkgSeq += [int64]$m.Groups[2].Value }

function Test-NonDecreasing($seq) {
    for ($i = 1; $i -lt $seq.Count; $i++) { if ($seq[$i] -lt $seq[$i-1]) { return $false } }
    return $true
}

$kibMono = Test-NonDecreasing $kibSeq
$pkgMono = Test-NonDecreasing $pkgSeq

$terminal     = [regex]::Match($serial, 'apt: index loaded \((\d+) packages\)')
$liveCount    = [regex]::Match($serial, 'LIVE_APT_UPDATE: count=(\d+)')
$footprint    = [regex]::Match($serial, 'Resident_Index_Footprint = (\d+) bytes')
$livePass     = $serial -match 'LXSELFTEST live_update PASS'
$liveFail     = [regex]::Match($serial, 'LXSELFTEST live_update FAIL[^\r\n]*')
$tlsWarn      = [regex]::Matches($serial, 'net::tls: HTTPS is INSECURE')

Write-Host "`n================ LIVE-UPDATE REPORT ================" -ForegroundColor Yellow
Write-Host ("Progress lines observed : {0}" -f $progress.Count)
if ($progress.Count -gt 0) {
    Write-Host ("Last progress line      : decompressed {0} KiB, parsed {1} packages" -f $kibSeq[-1], $pkgSeq[-1])
    Write-Host ("Decompressed KiB monotonic non-decreasing : {0}" -f $kibMono)
    Write-Host ("Parsed packages monotonic non-decreasing  : {0}" -f $pkgMono)
}
if ($terminal.Success) {
    Write-Host ("Terminal `apt: index loaded` reached      : YES ({0} packages)" -f $terminal.Groups[1].Value) -ForegroundColor Green
} else {
    Write-Host  "Terminal `apt: index loaded` reached      : NO (still progressing or blocked)" -ForegroundColor DarkYellow
}
if ($liveCount.Success) {
    $n = [int64]$liveCount.Groups[1].Value
    $ge = if ($n -ge 50000) { 'PASS (>= 50000)' } else { 'BELOW THRESHOLD (< 50000)' }
    Write-Host ("LIVE_APT_UPDATE count   : {0}  [{1}]" -f $n, $ge) -ForegroundColor Green
} else {
    Write-Host  "LIVE_APT_UPDATE count   : (not reached)" -ForegroundColor DarkYellow
}
if ($footprint.Success) { Write-Host ("Resident_Index_Footprint: {0} bytes" -f $footprint.Groups[1].Value) }
if ($tlsWarn.Count -gt 0) { Write-Host ("Insecure-TLS warning (R7.4): emitted x{0}" -f $tlsWarn.Count) -ForegroundColor Green }

if ($livePass) {
    Write-Host "`nLIVE RESULT: PASS (count >= 50000, install + run succeeded)" -ForegroundColor Green
} elseif ($liveFail.Success) {
    Write-Host ("`nLIVE RESULT: FAIL line on serial -> {0}" -f $liveFail.Value.Trim()) -ForegroundColor Red
} elseif (-not $reachedTerminal) {
    Write-Host "`nLIVE RESULT: PARTIAL (soft timeout; see monotonic-progress evidence above)." -ForegroundColor DarkYellow
    Write-Host "Per Q-A timing is soft and non-binding; a still-progressing run is an ACCEPTABLE outcome." -ForegroundColor DarkYellow
} else {
    Write-Host "`nLIVE RESULT: INCONCLUSIVE (no PASS/FAIL marker and no progress)." -ForegroundColor DarkYellow
}

# ---------------------------------------------------------------------------
# 6. Restore the DEFAULT (no-feature) release ELF into iso_root\pagh.elf
# ---------------------------------------------------------------------------
Write-Host "`n=== Restoring default (no-feature) release ELF ===" -ForegroundColor Cyan
cargo build --release
Link-Kernel $lld
Copy-Item $relElf iso_root\pagh.elf -Force
Write-Host 'Default release ELF restored into iso_root\pagh.elf'

if ($livePass) { exit 0 } else { exit 1 }
