# ZeroLink Automated Windows Daemon Verification Script
# Validates build integrity, cargo tests, runtime daemon launch, and simulated Android peer integration.

$ErrorActionPreference = "Stop"

Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "     ZeroLink Automated Windows Verification Suite" -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan

# 1. Run Cargo Workspace Tests
Write-Host "`n[1/4] Running Rust Workspace Tests..." -ForegroundColor Yellow
cargo test --workspace
if ($LASTEXITCODE -ne 0) {
    Write-Host "Cargo test failed!" -ForegroundColor Red
    exit 1
}
Write-Host " All unit tests passed!" -ForegroundColor Green

# 2. Check Release Binary
Write-Host "`n[2/4] Verifying Release Binary..." -ForegroundColor Yellow
$verificationTarget = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.codex-target\verification-build"))
$binPath = Join-Path $verificationTarget "release\zerolink-win.exe"
Write-Host "Building the current release binary..." -ForegroundColor Yellow
cargo build --release --package zerolink-win --target-dir $verificationTarget
if ($LASTEXITCODE -ne 0) {
    throw "Release build failed with exit code $LASTEXITCODE."
}
$binItem = Get-Item -LiteralPath $binPath
$sizeKb = [math]::Round($binItem.Length / 1024, 2)
Write-Host " Binary verified: $binPath ($sizeKb KB)" -ForegroundColor Green

# 3. Launch an isolated daemon for integration testing.
$node = Get-Command node -ErrorAction SilentlyContinue
if (-not $node) {
    throw "Node.js is required for the built-in-crypto simulator."
}
$runId = Get-Date -Format "yyyyMMdd-HHmmss"
$profileRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.codex-target\verification-$runId"))
$roamingPath = Join-Path $profileRoot "AppData\Roaming"
$logsPath = Join-Path $profileRoot "logs"
$syncRootPath = [System.IO.Path]::GetFullPath((Join-Path $profileRoot "ZeroLink Photos"))
$hydrationAuditPath = Join-Path $logsPath "hydration-audit.txt"
$hydrationTracePath = Join-Path $logsPath "hydration-trace.txt"
New-Item -ItemType Directory -Force -Path $roamingPath, $logsPath | Out-Null
$stdoutPath = Join-Path $logsPath "daemon.stdout.log"
$stderrPath = Join-Path $logsPath "daemon.stderr.log"
$portProbe = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$portProbe.Start()
$testPort = $portProbe.LocalEndpoint.Port
$portProbe.Stop()
$bulkPortProbe = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$bulkPortProbe.Start()
$bulkTestPort = $bulkPortProbe.LocalEndpoint.Port
$bulkPortProbe.Stop()
$previousEnvironment = @{
    APPDATA = $env:APPDATA
    USERPROFILE = $env:USERPROFILE
    ZEROLINK_BIND_ADDR = $env:ZEROLINK_BIND_ADDR
    ZEROLINK_PORT = $env:ZEROLINK_PORT
    ZEROLINK_SIMULATOR_TIMEOUT_SECONDS = $env:ZEROLINK_SIMULATOR_TIMEOUT_SECONDS
    ZEROLINK_TEST_SHUTDOWN_AFTER_SECONDS = $env:ZEROLINK_TEST_SHUTDOWN_AFTER_SECONDS
    ZEROLINK_TEST_BULK_PORT = $env:ZEROLINK_TEST_BULK_PORT
    ZEROLINK_TEST_SYNC_ROOT = $env:ZEROLINK_TEST_SYNC_ROOT
    ZEROLINK_TEST_HYDRATION_AUDIT = $env:ZEROLINK_TEST_HYDRATION_AUDIT
    ZEROLINK_TEST_HYDRATION_TRACE = $env:ZEROLINK_TEST_HYDRATION_TRACE
}
$proc = $null

try {
    $env:APPDATA = $roamingPath
    $env:USERPROFILE = $profileRoot
    $env:ZEROLINK_BIND_ADDR = "127.0.0.1"
    $env:ZEROLINK_PORT = [string]$testPort
    $env:ZEROLINK_TEST_BULK_PORT = [string]$bulkTestPort
    $env:ZEROLINK_TEST_SYNC_ROOT = $syncRootPath
    $env:ZEROLINK_TEST_HYDRATION_AUDIT = $hydrationAuditPath
    $env:ZEROLINK_TEST_HYDRATION_TRACE = $hydrationTracePath
    # Keep the approval window open for an action-time human check.
    $env:ZEROLINK_SIMULATOR_TIMEOUT_SECONDS = "3600"
    $env:ZEROLINK_TEST_SHUTDOWN_AFTER_SECONDS = "3600"

    Write-Host "`n[3/4] Launching isolated ZeroLink daemon on 127.0.0.1:$testPort..." -ForegroundColor Yellow
    $proc = Start-Process -FilePath $binPath -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
    Start-Sleep -Seconds 3
    if ($proc.HasExited) {
        throw "Daemon exited prematurely. See $stdoutPath and $stderrPath."
    }
    Write-Host " Daemon active (PID: $($proc.Id))" -ForegroundColor Green

    # 4. Run the Node.js simulator. It requires a typed code match before pairing.
    Write-Host "`n[4/4] Executing Android Peer Simulation..." -ForegroundColor Yellow
    & $node.Source (Join-Path $PSScriptRoot "simulate_android_peer.mjs")
    if ($LASTEXITCODE -ne 0) {
        throw "Peer simulation failed with exit code $LASTEXITCODE."
    }
    Write-Host " Peer simulation completed successfully!" -ForegroundColor Green
    if (-not (Test-Path -LiteralPath $hydrationAuditPath)) {
        throw "The simulator did not verify Cloud Files placeholder hydration."
    }
    $hydrationAudit = Get-Content -LiteralPath $hydrationAuditPath -Raw
    if ($hydrationAudit -notmatch '(?m)^HydrationVerified=true\r?$') {
        throw "The Cloud Files hydration audit does not record a successful byte comparison."
    }
    Write-Host " Cloud Files placeholder hydration matched the synthetic Android media bytes." -ForegroundColor Green

    # The simulator verifies Windows' reciprocal key-confirmation tag and sends the
    # encrypted first-pair approval. Confirm Windows persisted the DPAPI-protected
    # pairing secret before the finally block removes the isolated test profile.
    $pairedSecretPath = Join-Path $roamingPath "ZeroLink\paired-peer.dat"
    $pairedSecretBytes = 0
    for ($attempt = 0; $attempt -lt 50; $attempt++) {
        if (Test-Path -LiteralPath $pairedSecretPath) {
            $pairedSecretBytes = (Get-Item -LiteralPath $pairedSecretPath).Length
            if ($pairedSecretBytes -gt 0) { break }
        }
        Start-Sleep -Milliseconds 100
    }
    if ($pairedSecretBytes -le 0) {
        throw "Simulator pairing completed, but Windows did not persist a non-empty pairing secret."
    }
    $pairingAuditPath = Join-Path $logsPath "pairing-audit.txt"
    $pairingAudit = "PairingSecretPersisted=true`r`nProtectedSecretBytes=$pairedSecretBytes`r`n"
    [System.IO.File]::WriteAllText($pairingAuditPath, $pairingAudit)
    Write-Host " Windows persisted the protected pairing secret ($pairedSecretBytes bytes); cleanup will remove it." -ForegroundColor Green
}
catch {
    Write-Host "Verification failed: $_" -ForegroundColor Red
    exit 1
}
finally {
    if ($proc) {
        $running = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
        if ($running) {
            $proc.Refresh()
            $wsMb = [math]::Round($running.WorkingSet64 / (1024 * 1024), 2)
            Write-Host "`nDaemon Metrics at Exit: Memory WorkingSet = $wsMb MB" -ForegroundColor Cyan
            $closeRequested = $false
            try {
                $closeRequested = $proc.CloseMainWindow()
            } catch {
                Write-Warning "Could not request graceful daemon shutdown: $_"
            }
            if (-not $proc.WaitForExit(15000)) {
                if ($closeRequested) {
                    Write-Warning "The daemon did not close its Cloud Files connection in time; stopping the isolated process."
                } else {
                Write-Warning "The daemon has no closable main window; stopping the isolated process."
                }
                Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
            }
        }
    }
    $pairedSecretPath = Join-Path $roamingPath "ZeroLink\paired-peer.dat"
    if (Test-Path -LiteralPath $pairedSecretPath) {
        Remove-Item -LiteralPath $pairedSecretPath -Force
    }
    if (Test-Path -LiteralPath $syncRootPath) {
        try {
            Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
public static class ZeroLinkVerificationCfApi {
    [DllImport("cldapi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    public static extern int CfUnregisterSyncRoot(string syncRootPath);
}
'@ -ErrorAction SilentlyContinue
            $unregisterResult = [ZeroLinkVerificationCfApi]::CfUnregisterSyncRoot($syncRootPath)
            if ($unregisterResult -ne 0) {
                Write-Warning ("Could not unregister the isolated test sync root (HRESULT 0x{0:X8})." -f $unregisterResult)
            }
        }
        catch {
            Write-Warning "Could not unregister the isolated test sync root: $_"
        }
    }
    Write-Host "Daemon logs retained at $logsPath" -ForegroundColor Cyan
    foreach ($name in $previousEnvironment.Keys) {
        if ($null -eq $previousEnvironment[$name]) {
            Remove-Item "Env:$name" -ErrorAction SilentlyContinue
        } else {
            Set-Item "Env:$name" -Value $previousEnvironment[$name]
        }
    }
}

Write-Host "`n==========================================================" -ForegroundColor Green
Write-Host "     ZeroLink Windows Suite Verification Complete!" -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green

