#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

set -euo pipefail

CARGO="${CARGO:-cargo}"

die() {
  printf '%s\n' "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

require_command "$CARGO"
require_command git

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" \
  || die "not inside a Git repository"
cd "$repo_root"

printf '==> package target selection test\n'
./scripts/package-target-test.sh

printf '==> cargo build\n'
$CARGO build

printf '==> cargo clippy\n'
$CARGO clippy --all-targets -- -D warnings

printf '==> cargo fmt --check\n'
$CARGO fmt --check

printf 'local CI passed\n'
