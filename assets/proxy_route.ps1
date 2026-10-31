param(
    [int]$OwnerPid = 0,
    [string]$OwnerStart = '',
    [ValidatePattern('^[a-f0-9]{64}$')][string]$Nonce,
    [switch]$ValidateOnly,
    [switch]$CleanupOnly
)
$ErrorActionPreference = 'Stop'

function Get-RluRouteText([string]$Text, [bool]$Enabled, [bool]$IPv6) {
    $begin = '# BEGIN RL UPLOADER WEBSOCKET'
    $end = '# END RL UPLOADER WEBSOCKET'
    $pattern = '(?m)^# BEGIN RL UPLOADER WEBSOCKET\r?\n127\.0\.0\.1 ws\.rlpp\.psynet\.gg\r?\n(?:::1 ws\.rlpp\.psynet\.gg\r?\n)?# END RL UPLOADER WEBSOCKET(?:\r?\n|$)'
    $clean = [regex]::Replace($Text, $pattern, '')
    if ($clean.Contains($begin) -or $clean.Contains($end)) {
        throw 'Unrecognized RL Uploader hosts block; refusing to edit it.'
    }
    if (-not $Enabled) { return $clean }
    foreach ($line in ($clean -split '\r?\n')) {
        $fields = (($line -split '#', 2)[0].Trim() -split '\s+')
        if ($fields.Count -gt 1 -and $fields[1..($fields.Count-1)] -contains 'ws.rlpp.psynet.gg') {
            throw 'An existing hostname mapping conflicts with the replay proxy.'
        }
    }
    if ($clean.Length -gt 0 -and -not $clean.EndsWith("`n")) { $clean += "`r`n" }
    $block = "$begin`r`n127.0.0.1 ws.rlpp.psynet.gg`r`n"
    if ($IPv6) { $block += "::1 ws.rlpp.psynet.gg`r`n" }
    return $clean + $block + "$end`r`n"
}

function Set-RluRoute([bool]$Enabled, [bool]$IPv6) {
    $windowsDirectory = [Environment]::GetFolderPath('Windows')
    $hostsPath = [IO.Path]::GetFullPath((Join-Path $windowsDirectory 'System32\drivers\etc\hosts'))
    $directory = [IO.Path]::GetDirectoryName($hostsPath)
    # Latin-1 round-trips every original byte, including UTF-8 comments.
    $encoding = [Text.Encoding]::GetEncoding(28591)
    for ($attempt = 0; $attempt -lt 5; $attempt++) {
        $original = [IO.File]::ReadAllBytes($hostsPath)
        if ($original -contains 0) { throw 'Unsupported hosts-file encoding.' }
        $text = $encoding.GetString($original)
        $changed = Get-RluRouteText -Text $text -Enabled $Enabled -IPv6 $IPv6
        if ($changed -ceq $text) { return }
        $temporary = Join-Path $directory ('rl-uploader-' + [Guid]::NewGuid().ToString('N') + '.tmp')
        try {
            [IO.File]::WriteAllBytes($temporary, $encoding.GetBytes($changed))
            $current = [IO.File]::ReadAllBytes($hostsPath)
            if ([Convert]::ToBase64String($current) -cne [Convert]::ToBase64String($original)) { continue }
            # ReplaceFile preserves the destination's Windows file metadata.
            [IO.File]::Replace($temporary, $hostsPath, $null)
            Clear-DnsClientCache
            return
        } finally {
            if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
        }
    }
    throw 'Hosts changed concurrently; route was not installed.'
}

function Invoke-RluControl([string]$Path, [string]$Body = '') {
    $request = [Net.HttpWebRequest]::Create('http://127.0.0.1:47639' + $Path)
    $request.Proxy = $null
    $request.Timeout = 1500
    $request.ReadWriteTimeout = 1500
    $request.Headers.Add('X-RLU-Nonce', $Nonce)
    if ($Body) {
        $request.Method = 'POST'
        $request.ContentType = 'application/json'
        $bytes = [Text.Encoding]::UTF8.GetBytes($Body)
        $request.ContentLength = $bytes.Length
        $stream = $request.GetRequestStream()
        try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
    }
    $response = $request.GetResponse()
    try {
        $reader = [IO.StreamReader]::new($response.GetResponseStream())
        try { return $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
    } finally { $response.Dispose() }
}

if ($ValidateOnly) { return }
$administrator = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $administrator.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Administrator access is required for the single-host route.'
}
$mutex = [Threading.Mutex]::new($false, 'Global\RLUploaderWebSocketRoute')
$ownsMutex = $false
$owner = $null
try {
    try { $ownsMutex = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $ownsMutex = $true }
    if (-not $ownsMutex) { throw 'Another replay routing watchdog is already running.' }
    if ($CleanupOnly) {
        Set-RluRoute -Enabled $false -IPv6 $false
        return
    }
    if ($OwnerPid -le 0 -or $OwnerStart -notmatch '^\d{15,20}$' -or -not $Nonce) { throw 'Invalid owner identity.' }
    $owner = [Diagnostics.Process]::GetProcessById($OwnerPid)
    if ($owner.StartTime.ToUniversalTime().ToFileTimeUtc().ToString() -cne $OwnerStart) { throw 'Owner process changed.' }
    $null = $owner.Handle  # Keep a handle to the original process, not a recycled PID.
    Set-RluRoute -Enabled $false -IPv6 $false

    # Recovery runs only fixed, embedded cleanup code. The SYSTEM task never
    # executes a binary or script from a user-writable project directory.
    $cleanup = '$ErrorActionPreference = "Stop"; function Get-RluRouteText {' +
        ${function:Get-RluRouteText}.ToString() + '}; function Set-RluRoute {' +
        ${function:Set-RluRoute}.ToString() + '}; Set-RluRoute -Enabled $false -IPv6 $false'
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($cleanup))
    $taskName = 'RLUploaderRestoreWebSocketRoute'
    $description = 'RL Uploader: remove only its WebSocket hosts block.'
    $existing = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    if ($existing -and $existing.Description -cne $description) { throw 'Recovery task name is already in use.' }
    $powershellPath = Join-Path ([Environment]::GetFolderPath('Windows')) 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $action = New-ScheduledTaskAction -Execute $powershellPath -Argument "-NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand $encoded"
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 1) -StartWhenAvailable
    $null = Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description $description -Force
    $null = Invoke-RluControl '/route-status' '{"state":"ready"}'
    while (-not $owner.HasExited) {
        $enabled = $false
        $ipv6 = $false
        try {
            $health = Invoke-RluControl '/health'
            $gamePresent = ([Diagnostics.Process]::GetProcessesByName('RocketLeague').Count -gt 0) -or
                           ([Diagnostics.Process]::GetProcessesByName('RocketLeague_EAC').Count -gt 0)
            $enabled = ($health.ready -eq $true) -and $gamePresent
            $ipv6 = $health.ipv6 -eq $true
        } catch { $enabled = $false }
        Set-RluRoute -Enabled $enabled -IPv6 $ipv6
        try {
            $state = if ($enabled) { 'active' } else { 'ready' }
            $null = Invoke-RluControl '/route-status' ('{"state":"' + $state + '"}')
        } catch {}
        $null = $owner.WaitForExit(500)
    }
} catch {
    try { $null = Invoke-RluControl '/route-status' '{"state":"error"}' } catch {}
} finally {
    if ($ownsMutex) {
        try { Set-RluRoute -Enabled $false -IPv6 $false } catch {}
        $mutex.ReleaseMutex()
    }
    if ($null -ne $owner) { $owner.Dispose() }
    $mutex.Dispose()
}
