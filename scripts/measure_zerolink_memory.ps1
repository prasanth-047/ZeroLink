param(
    [ValidateRange(1, 10)]
    [int]$Iterations = 3,
    [ValidateRange(6, 120)]
    [int]$MeasureAfterSeconds = 8
)

$ErrorActionPreference = "Stop"
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$binaryPath = Join-Path $repoRoot "target\release\zerolink-win.exe"
if (-not (Test-Path -LiteralPath $binaryPath)) {
    throw "Release binary not found: $binaryPath. Build zerolink-win in release mode first."
}

$reportId = Get-Date -Format "yyyyMMdd-HHmmss"
$reportRoot = Join-Path $repoRoot ".codex-target\memory-benchmark-$reportId"
New-Item -ItemType Directory -Force -Path $reportRoot | Out-Null

$previousEnvironment = @{}
foreach ($name in "APPDATA", "USERPROFILE", "ZEROLINK_BIND_ADDR", "ZEROLINK_PORT", "ZEROLINK_TEST_SHUTDOWN_AFTER_SECONDS") {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
}

if (-not ("ZeroLinkMemoryBenchmarkCfApi" -as [type])) {
    Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
public static class ZeroLinkMemoryBenchmarkCfApi {
  [DllImport("cldapi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
  public static extern int CfUnregisterSyncRoot(string syncRootPath);
}
'@
}

$results = [System.Collections.Generic.List[object]]::new()
try {
    for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
        $iterationRoot = Join-Path $reportRoot "iteration-$iteration"
        $roamingPath = Join-Path $iterationRoot "AppData\Roaming"
        $logsPath = Join-Path $iterationRoot "logs"
        $syncRootPath = [System.IO.Path]::GetFullPath((Join-Path $iterationRoot "ZeroLink Photos"))
        New-Item -ItemType Directory -Force -Path $roamingPath, $logsPath | Out-Null

        $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
        $process = $null
        try {
            $listener.Start()
            $port = $listener.LocalEndpoint.Port
            $listener.Stop()

            $env:APPDATA = $roamingPath
            $env:USERPROFILE = $iterationRoot
            $env:ZEROLINK_BIND_ADDR = "127.0.0.1"
            $env:ZEROLINK_PORT = [string]$port
            $env:ZEROLINK_TEST_SHUTDOWN_AFTER_SECONDS = [string]($MeasureAfterSeconds + 5)

            $process = Start-Process `
                -FilePath $binaryPath `
                -PassThru `
                -WindowStyle Hidden `
                -RedirectStandardOutput (Join-Path $logsPath "daemon.stdout.log") `
                -RedirectStandardError (Join-Path $logsPath "daemon.stderr.log")

            Start-Sleep -Seconds $MeasureAfterSeconds
            $process.Refresh()
            if ($process.HasExited) {
                throw "Daemon exited before measurement. Inspect logs at $logsPath."
            }

            $measurement = Get-Process -Id $process.Id
            $results.Add([pscustomobject]@{
                Iteration = $iteration
                WorkingSetMB = [math]::Round($measurement.WorkingSet64 / 1MB, 2)
                PrivateMB = [math]::Round($measurement.PrivateMemorySize64 / 1MB, 2)
                ThreadCount = $measurement.Threads.Count
                MeasureAfterSeconds = $MeasureAfterSeconds
                ConnectedPeer = $false
            })
        }
        finally {
            if ($listener.Server.IsBound) {
                $listener.Stop()
            }
            if ($process) {
                $process.Refresh()
                if (-not $process.HasExited -and -not $process.WaitForExit(15000)) {
                    Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
                    throw "Daemon did not shut down cleanly; its sync root may still be connected. Inspect logs at $logsPath."
                }
            }
            if (Test-Path -LiteralPath $syncRootPath) {
                $unregisterResult = [ZeroLinkMemoryBenchmarkCfApi]::CfUnregisterSyncRoot($syncRootPath)
                if ($unregisterResult -ne 0) {
                    Write-Warning ("Could not unregister isolated test sync root (HRESULT 0x{0:X8})." -f $unregisterResult)
                }
            }
        }
    }
}
finally {
    foreach ($name in $previousEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], "Process")
    }
}

$csvPath = Join-Path $reportRoot "measurements.csv"
$results | Export-Csv -LiteralPath $csvPath -NoTypeInformation
$results | Format-Table -AutoSize
Write-Output "Measurements and isolated logs: $reportRoot"
