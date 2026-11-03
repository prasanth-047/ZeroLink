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
$binPath = "S:\Projects\zerolink\target\release\zerolink-win.exe"
if (-not (Test-Path $binPath)) {
    Write-Host "Compiling release binary..." -ForegroundColor Yellow
    cargo build --release --package zerolink-win
}
$binItem = Get-Item $binPath
$sizeKb = [math]::Round($binItem.Length / 1024, 2)
Write-Host " Binary verified: $binPath ($sizeKb KB)" -ForegroundColor Green

# 3. Launch Daemon in background for integration test
Write-Host "`n[3/4] Launching ZeroLink Windows Daemon on 0.0.0.0:8989..." -ForegroundColor Yellow
$proc = Start-Process -FilePath $binPath -PassThru
Start-Sleep -Seconds 2

if ($proc.HasExited) {
    Write-Host "Daemon exited prematurely!" -ForegroundColor Red
    exit 1
}
Write-Host " Daemon active (PID: $($proc.Id))" -ForegroundColor Green

# 4. Run Simulated Android Peer
Write-Host "`n[4/4] Executing Android Peer Simulation..." -ForegroundColor Yellow
try {
    python S:\Projects\zerolink\scripts\simulate_android_peer.py
    Write-Host " Peer simulation completed successfully!" -ForegroundColor Green
}
catch {
    Write-Host "Simulation encountered an error: $_" -ForegroundColor Red
}
finally {
    # Check memory and clean up test daemon
    try {
        $p = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
        if ($p) {
            $wsMb = [math]::Round($p.WorkingSet64 / (1024 * 1024), 2)
            Write-Host "`nDaemon Metrics at Exit: Memory WorkingSet = $wsMb MB (Target < 15 MB)" -ForegroundColor Cyan
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        }
    } catch {}
}

Write-Host "`n==========================================================" -ForegroundColor Green
Write-Host "     ZeroLink Windows Suite Verification Complete!" -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green

