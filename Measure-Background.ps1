param(
    [ValidateRange(5, 300)][int]$Seconds = 30,
    [string]$Executable = (Join-Path $env:LOCALAPPDATA 'Programs\RLUploader\rl-uploader.exe')
)
$ErrorActionPreference = 'Stop'
$appPath = [IO.Path]::GetFullPath($Executable)
function Get-TrayProcess {
    $matching = @(Get-Process rl-uploader -ErrorAction SilentlyContinue | Where-Object { $_.Path -ieq $appPath })
    if ($matching.Count -ne 1) {
        throw 'Run the tray app and close its settings window before measuring. Exactly one app process is required.'
    }
    return $matching[0]
}
$initial = Get-TrayProcess
$sampleProcessId = $initial.Id
$initialCpu = $initial.TotalProcessorTime.TotalMilliseconds
$timer = [Diagnostics.Stopwatch]::StartNew()
Start-Sleep -Seconds $Seconds
$final = Get-TrayProcess
if ($final.Id -ne $sampleProcessId) { throw 'The app restarted during measurement; try again.' }
$elapsed = $timer.Elapsed.TotalSeconds
$cpuMs = $final.TotalProcessorTime.TotalMilliseconds - $initialCpu
[pscustomobject]@{
    Seconds = [math]::Round($elapsed, 3)
    CpuMilliseconds = [math]::Round($cpuMs, 3)
    CpuPercentOfOneCore = [math]::Round($cpuMs / ($elapsed * 10), 4)
    WorkingSetMiB = [math]::Round($final.WorkingSet64 / 1MB, 2)
    PrivateMemoryMiB = [math]::Round($final.PrivateMemorySize64 / 1MB, 2)
    Threads = $final.Threads.Count
} | ConvertTo-Json
