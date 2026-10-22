# Logs Runehaven's memory every few seconds into a CSV file on the desktop.
# Usage (PowerShell): start Runehaven, then run
#   powershell -ExecutionPolicy Bypass -File runehaven-memlog.ps1
# Stops automatically when the game exits. Part of RML (development tool).
param(
    [int]$IntervalSeconds = 10,
    [string]$ProcessName = "Runehaven"
)

$out = Join-Path ([Environment]::GetFolderPath("Desktop")) ("runehaven-memlog-" + (Get-Date -Format "yyyyMMdd-HHmmss") + ".csv")
"time,seconds,private_mb,working_set_mb,gpu_dedicated_mb,gpu_shared_mb" | Out-File -FilePath $out -Encoding ascii

Write-Host "Waiting for $ProcessName ..."
$proc = $null
while (-not $proc) {
    $proc = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $proc) { Start-Sleep -Seconds 2 }
}
Write-Host "Logging $ProcessName (PID $($proc.Id)) to $out - press Ctrl+C to stop."

function Get-GpuMb([int]$processId, [string]$counter) {
    try {
        $samples = (Get-Counter "\GPU Process Memory(pid_$($processId)_*)\$counter" -ErrorAction Stop).CounterSamples
        return [math]::Round((($samples | Measure-Object -Property CookedValue -Sum).Sum) / 1MB, 1)
    } catch {
        return ""
    }
}

$start = Get-Date
while ($true) {
    $proc.Refresh()
    if ($proc.HasExited) { break }
    $line = "{0},{1},{2},{3},{4},{5}" -f (Get-Date -Format "HH:mm:ss"),
        [int]((Get-Date) - $start).TotalSeconds,
        [math]::Round($proc.PrivateMemorySize64 / 1MB, 1),
        [math]::Round($proc.WorkingSet64 / 1MB, 1),
        (Get-GpuMb $proc.Id "Dedicated Usage"),
        (Get-GpuMb $proc.Id "Shared Usage")
    $line | Out-File -FilePath $out -Append -Encoding ascii
    Write-Host $line
    Start-Sleep -Seconds $IntervalSeconds
}
Write-Host "Game exited. Log: $out"
