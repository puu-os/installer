#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

set -euo pipefail

MESON="${MESON:-meson}"
BUILDDIR="${BUILDDIR:-build}"
SANITIZE_BUILDDIR="${SANITIZE_BUILDDIR:-build-sanitize}"
CLANG_FORMAT="${CLANG_FORMAT:-clang-format}"

die() {
  printf '%s\n' "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

require_command "$MESON"
require_command ninja
require_command git

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" \
  || die "not inside a Git repository"
cd "$repo_root"

printf '==> sha256 sidecar test\n'
./scripts/sha256-test.sh

printf '==> meson setup\n'
if [[ -d "$BUILDDIR" ]]; then
  "$MESON" setup --reconfigure "$BUILDDIR" --wipe
else
  "$MESON" setup "$BUILDDIR"
fi

printf '==> build\n'
ninja -C "$BUILDDIR"

printf '==> test\n'
"$MESON" test -C "$BUILDDIR" --print-errorlogs

if command -v "$CLANG_FORMAT" >/dev/null 2>&1; then
  printf '==> clang-format --dry-run\n'
  # Only our own sources; subprojects follow their upstream style.
  find src \( -name '*.cc' -o -name '*.h' \) -print0 \
    | xargs -0 "$CLANG_FORMAT" --dry-run --Werror
else
  printf 'clang-format not found; skipping the format check\n' >&2
fi

# The strictest static analysis gate is opt-in upstream but must run here.
if command -v clang-tidy >/dev/null 2>&1; then
  printf '==> clang-tidy\n'
  ninja -C "$BUILDDIR" clang-tidy
else
  printf 'clang-tidy not found; skipping the lint check\n' >&2
fi

if command -v shellcheck >/dev/null 2>&1; then
  printf '==> shellcheck\n'
  git ls-files '*.sh' | xargs shellcheck
else
  printf 'shellcheck not found; skipping the shell lint check\n' >&2
fi

printf '==> asan/ubsan build and test\n'
# AddressSanitizer plus UndefinedBehaviorSanitizer over the same test suite;
# halt_on_error makes the first finding fail the run.
ASAN_OPTIONS=halt_on_error=1 UBSAN_OPTIONS=halt_on_error=1 \
  "$(dirname "$0")/sanitize-ci.sh"

printf 'local CI passed\n'
