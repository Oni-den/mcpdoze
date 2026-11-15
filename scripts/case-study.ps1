# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
  Measure the memory saved by MCPDoze: N agent sessions each running their own
  copy of every MCP server (baseline) versus N sessions sharing one broker.

.DESCRIPTION
  Windows / PowerShell 7. Runs entirely in a temporary directory; it does not read
  or modify any agent or MCPDoze configuration. Tool results are discarded
  (only success/failure is recorded), so no server output reaches the report.

.PARAMETER Binary    Path to the mcpdoze executable.
.PARAMETER ServersFile  JSON array: [{ "name": "x", "command": "npx", "args": ["-y","pkg"],
                     "tool": "some_tool", "arguments": {} }, ...]  ("tool" is a harmless
                     read-only tool used to make the server do real work.)
.PARAMETER Sessions  Number of simulated agent sessions (default 2).
.PARAMETER TtlSeconds  Broker idle TTL used for the run (default 20).
.PARAMETER Port      Loopback port for the broker (default 17878).
#>
param(
    [Parameter(Mandatory)] [string] $Binary,
    [Parameter(Mandatory)] [string] $ServersFile,
    [int] $Sessions = 2,
    [int] $TtlSeconds = 20,
    [int] $Port = 17878
)
$ErrorActionPreference = 'Stop'
$servers = Get-Content -Raw $ServersFile | ConvertFrom-Json

function Get-Descendants([int] $RootPid) {
    $all = Get-CimInstance Win32_Process
    $kids = @{}
    foreach ($p in $all) { $kids[[int]$p.ParentProcessId] += @($p) }
    $out = New-Object System.Collections.Generic.List[object]
    $stack = New-Object System.Collections.Generic.Stack[int]
    $stack.Push($RootPid)
    while ($stack.Count) {
        $cur = $stack.Pop()
        foreach ($c in $kids[$cur]) { $out.Add($c); $stack.Push([int]$c.ProcessId) }
    }
    $out
}
function Measure-Tree([int[]] $Roots, [switch] $IncludeRoots) {
    $procs = @()
    foreach ($r in $Roots) {
        if ($IncludeRoots) { $procs += @(Get-CimInstance Win32_Process -Filter "ProcessId=$r") }
        $procs += @(Get-Descendants $r)
    }
    $procs = $procs | Where-Object { $_ } | Sort-Object ProcessId -Unique
    [pscustomobject]@{
        Processes = @($procs).Count
        PrivateMB = [math]::Round((($procs | Measure-Object PrivatePageCount -Sum).Sum) / 1MB, 1)
    }
}
function Start-Stdio($server) {
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $resolved = (Get-Command $server.command -CommandType Application -ErrorAction Stop |
        Where-Object { $_.Source -match '\.(exe|cmd|bat|com)$' } | Select-Object -First 1).Source
    if ($resolved -match '\.(cmd|bat)$') {
        # Windows shims (npx, firebase, ...) must run through cmd.exe, as an agent would.
        # `cmd /s /c ""path" args"` is the form that survives spaces in the path.
        $q = { param($x) if ($x -match '[\s"]') { '"' + ($x -replace '"', '\"') + '"' } else { $x } }
        $inner = (@('"' + $resolved + '"') + @($server.args | ForEach-Object { & $q ([string]$_) })) -join ' '
        $psi.FileName = 'cmd.exe'
        $psi.Arguments = '/d /s /c "' + $inner + '"'
    } else {
        $psi.FileName = $resolved
        foreach ($a in $server.args) { $psi.ArgumentList.Add([string]$a) }
    }
    $psi.RedirectStandardInput = $true; $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true; $psi.UseShellExecute = $false; $psi.CreateNoWindow = $true
    [System.Diagnostics.Process]::Start($psi)
}
function Send-Line($proc, $obj) { $proc.StandardInput.WriteLine(($obj | ConvertTo-Json -Compress -Depth 20)); $proc.StandardInput.Flush() }
function Read-Reply($proc, [int] $Id, [int] $TimeoutSec = 90) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSec)
    while ([DateTime]::UtcNow -lt $deadline) {
        # Keep ONE pending read per process; starting a second one while the first is
        # outstanding would lose lines.
        if (-not $proc.PSObject.Properties['Pending'] -or $null -eq $proc.Pending) {
            $proc | Add-Member -NotePropertyName Pending -NotePropertyValue $proc.StandardOutput.ReadLineAsync() -Force
        }
        if (-not $proc.Pending.Wait([TimeSpan]::FromSeconds(5))) { continue }
        $line = $proc.Pending.Result; $proc.Pending = $null
        if ($null -eq $line) { throw 'server closed stdout' }
        try { $m = $line | ConvertFrom-Json } catch { continue }
        if ($m.id -eq $Id) { return $m }
    }
    throw "timeout waiting for reply $Id"
}

$rows = @()
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("mcpdoze-case-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory $tmp | Out-Null

# ---------------- Baseline: every session spawns its own copy of every server
Write-Host "Baseline: $Sessions sessions x $($servers.Count) servers, direct stdio"
$base = @()
try {
    for ($s = 1; $s -le $Sessions; $s++) {
        foreach ($srv in $servers) {
            $p = Start-Stdio $srv; $base += $p
            Send-Line $p @{ jsonrpc = '2.0'; id = 1; method = 'initialize'; params = @{ protocolVersion = '2025-11-25'; capabilities = @{}; clientInfo = @{ name = 'case-study'; version = '0' } } }
            [void](Read-Reply $p 1)
            Send-Line $p @{ jsonrpc = '2.0'; method = 'notifications/initialized' }
            Send-Line $p @{ jsonrpc = '2.0'; id = 2; method = 'tools/call'; params = @{ name = $srv.tool; arguments = $srv.arguments } }
            [void](Read-Reply $p 2)
        }
    }
    Start-Sleep -Seconds 3
    $m = Measure-Tree ($base | ForEach-Object { $_.Id }) -IncludeRoots
    $rows += [pscustomobject]@{ Scenario = "Baseline: $Sessions sessions, own copies"; Processes = $m.Processes; PrivateMB = $m.PrivateMB }
}
finally {
    foreach ($p in $base) { if (-not $p.HasExited) { & taskkill /PID $p.Id /T /F | Out-Null } }
}
Start-Sleep -Seconds 3

# ---------------- MCPDoze: N sessions through one broker
$cfg = Join-Path $tmp 'config.toml'
$toml = @"
[broker]
bind = "127.0.0.1:$Port"
token_file = '$($tmp -replace "'", "''")\token'
catalog_dir = '$($tmp -replace "'", "''")\catalog'
default_idle_ttl = "${TtlSeconds}s"

"@
foreach ($srv in $servers) {
    $argList = ($srv.args | ForEach-Object { '"' + ($_ -replace '\\', '\\' -replace '"', '\"') + '"' }) -join ', '
    $toml += "[servers.$($srv.name)]`ntransport = `"stdio`"`ncommand = '$($srv.command)'`nargs = [$argList]`nprobe = false`n`n"
}
Set-Content -Encoding utf8NoBOM $cfg $toml

$broker = $null
try {
    $broker = Start-Process -FilePath $Binary -ArgumentList @('serve', '--config', $cfg, '--no-watch') -PassThru -WindowStyle Hidden
    $base_url = "http://127.0.0.1:$Port"
    for ($i = 0; $i -lt 50; $i++) { try { if ((Invoke-RestMethod "$base_url/healthz") -match 'ok') { break } } catch { Start-Sleep -Milliseconds 200 } }
    $token = (Get-Content -Raw (Join-Path $tmp 'token')).Trim()
    $hdr = @{ Authorization = "Bearer $token"; Accept = 'application/json, text/event-stream' }

    $idle = Measure-Tree @($broker.Id) -IncludeRoots
    $rows += [pscustomobject]@{ Scenario = 'MCPDoze: broker started, no calls'; Processes = $idle.Processes; PrivateMB = $idle.PrivateMB }

    $ok = 0; $fail = 0
    for ($s = 1; $s -le $Sessions; $s++) {
        $init = Invoke-WebRequest "$base_url/mcp" -Method Post -Headers $hdr -ContentType 'application/json' -Body (@{ jsonrpc = '2.0'; id = 1; method = 'initialize'; params = @{ protocolVersion = '2025-11-25'; capabilities = @{}; clientInfo = @{ name = "case-study-$s"; version = '0' } } } | ConvertTo-Json -Compress -Depth 20)
        $sid = $init.Headers['Mcp-Session-Id'] | Select-Object -First 1
        $h2 = $hdr + @{ 'Mcp-Session-Id' = $sid }
        [void](Invoke-WebRequest "$base_url/mcp" -Method Post -Headers $h2 -ContentType 'application/json' -Body '{"jsonrpc":"2.0","method":"notifications/initialized"}')
        [void](Invoke-WebRequest "$base_url/mcp" -Method Post -Headers $h2 -ContentType 'application/json' -Body '{"jsonrpc":"2.0","id":2,"method":"tools/list"}')
        $n = 10
        foreach ($srv in $servers) {
            $n++
            $body = @{ jsonrpc = '2.0'; id = $n; method = 'tools/call'; params = @{ name = "$($srv.name)__$($srv.tool)"; arguments = $srv.arguments } } | ConvertTo-Json -Compress -Depth 20
            $r = (Invoke-WebRequest "$base_url/mcp" -Method Post -Headers $h2 -ContentType 'application/json' -Body $body).Content | ConvertFrom-Json
            if ($r.error) { $fail++ } else { $ok++ }
        }
    }
    Write-Host "Broker calls: $ok ok, $fail JSON-RPC errors"
    Start-Sleep -Seconds 3
    $act = Measure-Tree @($broker.Id) -IncludeRoots
    $rows += [pscustomobject]@{ Scenario = "MCPDoze: $Sessions sessions active"; Processes = $act.Processes; PrivateMB = $act.PrivateMB }

    Write-Host "Waiting $($TtlSeconds + 15) s for idle unload..."
    Start-Sleep -Seconds ($TtlSeconds + 15)
    $after = Measure-Tree @($broker.Id) -IncludeRoots
    $rows += [pscustomobject]@{ Scenario = 'MCPDoze: after idle TTL'; Processes = $after.Processes; PrivateMB = $after.PrivateMB }
}
finally {
    if ($broker -and -not $broker.HasExited) { & taskkill /PID $broker.Id /T /F | Out-Null }
    Start-Sleep -Seconds 1
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Host ''
$rows | Format-Table -AutoSize
Write-Host '| Scenario | Processes | Private memory (MB) |'
Write-Host '|---|---:|---:|'
foreach ($r in $rows) { Write-Host "| $($r.Scenario) | $($r.Processes) | $($r.PrivateMB) |" }
