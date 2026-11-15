# SPDX-License-Identifier: GPL-3.0-or-later
# Dry-run `mcpdoze import` for every supported agent. Changes nothing.
$ErrorActionPreference = 'Continue'
if (-not (Get-Command mcpdoze -ErrorAction SilentlyContinue)) {
    Write-Error 'mcpdoze not found on PATH'; exit 1
}
foreach ($agent in 'claude-code', 'codex', 'opencode', 'cursor', 'vscode') {
    Write-Output "=== $agent ==="
    mcpdoze import $agent --dry-run
}
