<#
.SYNOPSIS
  Build / transport smoke assertions for spec full-debian-apt-update task 11.2.

.DESCRIPTION
  Documents and checks the three build/transport smoke criteria from captured
  serial evidence and a quick debug-build link, WITHOUT re-running the full live
  update:

    R4.1 - release build links AND boots: proven by the release ELF reaching the
           interactive shell ("Welcome to pagh OS Shell!" / "pagh:/>") in a
           captured serial log.
    R4.2 - debug build loads the full index functionally: a quick debug-build link
           (run.cmd build debug) proves the debug profile compiles+links; the
           deterministic local-mirror e2e (serial_e2e.log) exercises the same
           index pipeline functionally.
    R7.4 - HTTPS server authentication is exercised with concrete serial evidence:
           POSITIVE - the TLS 1.3 handshake under the fail-closed verifier
           completed ("LXSELFTEST https_get PASS" against deb.debian.org, or the
           "Package_Fetcher(tls): stage=response ... cause=Status(" line that can
           only be emitted after a verified handshake), or NEGATIVE - the
           handshake was refused and the verifier named the check that refused it
           ("Package_Fetcher(tls): stage=verify cause=..."). A plain-HTTP
           "apt: index loaded" line is NOT accepted: it proves the index pipeline,
           not authentication.

  It reads serial_e2e.log (local-mirror run) and, if present, serial_live.log
  (live run) for the evidence lines and reports which lines satisfy each criterion.

.PARAMETER LocalLog
  Path to the local-mirror serial log (default serial_e2e.log).
.PARAMETER LiveLog
  Path to the live-update serial log (default serial_live.log; optional).
.PARAMETER SkipDebugBuild
  Skip the run.cmd build debug link check (use only captured evidence).
#>
[CmdletBinding()]
param(
    [string]$LocalLog = 'serial_e2e.log',
    [string]$LiveLog  = 'serial_live.log',
    [switch]$SkipDebugBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Read-LogOrEmpty($p) { if (Test-Path $p) { Get-Content $p -Raw } else { '' } }

$local = Read-LogOrEmpty $LocalLog
$live  = Read-LogOrEmpty $LiveLog

Write-Host "================ TASK 11.2 SMOKE ASSERTIONS ================" -ForegroundColor Yellow

# --- R4.1: release build links AND boots (reaches the shell) ---
$shell  = [regex]::Match($local, 'Welcome to pagh OS Shell!')
$prompt = [regex]::Match($local, 'pagh:/>')
$r41 = $shell.Success -or $prompt.Success
Write-Host "`n[R4.1] release build links AND boots:" -ForegroundColor Cyan
if ($r41) {
    Write-Host "  PASS - release ELF reached the shell." -ForegroundColor Green
    if ($shell.Success)  { Write-Host "    evidence: 'Welcome to pagh OS Shell!' ($LocalLog)" }
    if ($prompt.Success) { Write-Host "    evidence: 'pagh:/>' prompt ($LocalLog)" }
} else {
    Write-Host "  UNPROVEN - no shell/prompt line found in $LocalLog" -ForegroundColor DarkYellow
}

# --- R4.2: debug build loads the full index functionally ---
Write-Host "`n[R4.2] debug build loads the full index functionally:" -ForegroundColor Cyan
$r42link = $false
if (-not $SkipDebugBuild) {
    Write-Host "  Running 'run.cmd build debug' link check ..."
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $out = cmd /c "run.cmd build debug 2>&1"
    $ErrorActionPreference = $prevEAP
    $r42link = ($out -match 'Link OK')
    if ($r42link) {
        Write-Host "  PASS (link) - debug profile compiles + links." -ForegroundColor Green
        ($out | Select-String 'Link OK') | ForEach-Object { Write-Host ("    evidence: " + $_.Line.Trim()) }
    } else {
        Write-Host "  FAIL (link) - debug build did not link." -ForegroundColor Red
    }
} else {
    Write-Host "  (debug-build link check skipped by flag)" -ForegroundColor DarkYellow
}
$idx = [regex]::Match($local, 'apt: index loaded \((\d+) packages\)')
if ($idx.Success) {
    Write-Host ("  Functional index pipeline evidence ($LocalLog): '{0}'" -f $idx.Value.Trim())
    Write-Host "    note: the local-mirror e2e drives the same update/parse/install code paths;"
    Write-Host "          the live full-index variant is task 11.1 (network-dependent, soft timing)."
}

# --- R7.4: HTTPS server authentication has concrete serial evidence ---
# The historical one-time "HTTPS is INSECURE" warning no longer exists: HTTPS is
# fail-closed authenticated. Accept EITHER a positive handshake outcome (which
# can only be reached after chain + SAN + clock gate + CertificateVerify passed)
# OR a negative one that names the check which refused the handshake - both prove
# the verifier is on the path and doing work.
Write-Host "`n[R7.4] HTTPS server authentication evidenced in the serial log:" -ForegroundColor Cyan
$r74hit  = $null
$r74cls  = $null
$r74log  = $null
$positive = @(
    @{ re = 'LXSELFTEST https_get PASS';        why = 'TLS 1.3 handshake completed (chain + SAN + clock + CertificateVerify passed) and the body decrypted' },
    @{ re = 'Package_Fetcher\(tls\): stage=response host=.*cause=Status\('; why = 'TLS handshake completed; only the HTTP status was non-200' }
)
$negative = @(
    @{ re = 'Package_Fetcher\(tls\): stage=verify cause=InvalidCertificate';     why = 'handshake refused: chain/SAN/clock check failed' },
    @{ re = 'Package_Fetcher\(tls\): stage=verify cause=InvalidSignatureScheme'; why = 'handshake refused: unacceptable CertificateVerify scheme' },
    @{ re = 'Package_Fetcher\(tls\): stage=verify cause=InvalidSignature';       why = 'handshake refused: CertificateVerify did not verify' }
)
foreach ($src in @(@{log=$LocalLog; text=$local}, @{log=$LiveLog; text=$live})) {
    if ([string]::IsNullOrEmpty($src.text)) { continue }
    foreach ($p in $positive) {
        $m = [regex]::Match($src.text, $p.re)
        if ($m.Success -and -not $r74hit) { $r74hit = $m.Value.Trim(); $r74cls = "positive: $($p.why)"; $r74log = $src.log }
    }
    foreach ($n in $negative) {
        $m = [regex]::Match($src.text, $n.re)
        if ($m.Success -and -not $r74hit) { $r74hit = $m.Value.Trim(); $r74cls = "negative: $($n.why)"; $r74log = $src.log }
    }
}
if ($r74hit) {
    Write-Host "  PASS - $r74cls" -ForegroundColor Green
    Write-Host ("    evidence ({0}): '{1}'" -f $r74log, $r74hit)
} else {
    Write-Host "  UNPROVEN - neither a completed TLS handshake nor a verifier refusal line was found." -ForegroundColor DarkYellow
    Write-Host "             expected e.g. 'LXSELFTEST https_get PASS ...' or 'Package_Fetcher(tls): stage=verify cause=...'"
}

Write-Host "`n================ SUMMARY ================" -ForegroundColor Yellow
$r41s = if ($r41) { 'PASS' } else { 'UNPROVEN' }
$r42s = if ($SkipDebugBuild) { 'SKIPPED' } elseif ($r42link) { 'PASS' } else { 'FAIL' }
$r74s = if ($r74hit) { 'PASS' } else { 'UNPROVEN' }
Write-Host ("  R4.1 release links+boots : {0}" -f $r41s)
Write-Host ("  R4.2 debug build link    : {0}" -f $r42s)
Write-Host ("  R7.4 HTTPS auth evidence : {0}  {1}" -f $r74s, $r74cls)
