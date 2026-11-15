#Requires -Version 7.0
# SPDX-License-Identifier: GPL-3.0-or-later
#
# PowerShell twin of pii-lint.sh. Scans tracked files (git ls-files) for user
# profile paths, non-allowed email addresses, provider token prefixes and
# private IPv4 addresses. Prints file:line and the rule, never the matched text.
# Exits 1 when any finding is reported.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = (git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0) { exit 2 }
Set-Location -LiteralPath $root

$excluded = @('LICENSE', 'scripts/pii-lint.sh', 'scripts/pii-lint.ps1')
$files = @(git ls-files | Where-Object { $_ -notin $excluded })

$allowedUserNames = @('example', 'user', 'runner', 'username')

$rules = @(
    [pscustomobject]@{
        Name    = 'user-profile path'
        Pattern = '(?i)(?:[A-Za-z]:[\\/]+Users[\\/]+|/Users/|/home/)([^\\/\s"''`<>]*)'
        Allowed = {
            param($m)
            $name = $m.Groups[1].Value
            $name -eq '' -or $name.StartsWith('<') -or ($allowedUserNames -contains $name.ToLowerInvariant())
        }
    }
    [pscustomobject]@{
        Name    = 'email address'
        Pattern = '(?i)[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'
        Allowed = {
            param($m)
            $value = $m.Value.ToLowerInvariant()
            $at = $value.IndexOf('@')
            $localPart = $value.Substring(0, $at)
            $domain = $value.Substring($at + 1)
            $localPart.Contains('noreply') -or ($domain -in @('example.com', 'users.noreply.github.com'))
        }
    }
    [pscustomobject]@{
        Name    = 'secret-looking token'
        Pattern = '(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|(?<![A-Za-z0-9])sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}'
        Allowed = { param($m) $false }
    }
    [pscustomobject]@{
        Name    = 'private IP address'
        Pattern = '(?<![0-9.])(10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3})(?![0-9.])'
        Allowed = { param($m) $false }
    }
)

$failures = 0
foreach ($file in $files) {
    $bytes = [IO.File]::ReadAllBytes((Join-Path $root $file))
    if ($bytes -contains 0) { continue }   # skip binaries, as grep -I does
    $lines = [Text.Encoding]::UTF8.GetString($bytes) -split "`n"
    for ($i = 0; $i -lt $lines.Count; $i++) {
        foreach ($rule in $rules) {
            foreach ($m in [regex]::Matches($lines[$i], $rule.Pattern)) {
                if (& $rule.Allowed $m) { continue }
                Write-Output "${file}:$($i + 1): $($rule.Name)"
                $failures++
                break
            }
        }
    }
}

if ($failures -gt 0) {
    [Console]::Error.WriteLine('pii-lint: findings above. Use placeholders (<home>, example) and remove secrets.')
    exit 1
}
Write-Output 'pii-lint: clean'
