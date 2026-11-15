#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Dry-run `mcpdoze import` for every supported agent. Changes nothing.
command -v mcpdoze >/dev/null 2>&1 || { echo "mcpdoze not found on PATH" >&2; exit 1; }
for agent in claude-code codex opencode cursor vscode; do
    echo "=== $agent ==="
    mcpdoze import "$agent" --dry-run
done
