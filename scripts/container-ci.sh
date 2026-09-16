#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

set -euo pipefail

CMAKE="${CMAKE:-cmake}"
CTEST="${CTEST:-ctest}"
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

for executable in "$CMAKE" "$CTEST" ninja git; do
  require_command "$executable"
done

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" \
  || die "not inside a Git repository"
cd "$repo_root"

printf '==> cmake configure\n'
# --fresh discards any stale cache from an earlier configuration.
"$CMAKE" -S . -B "$BUILDDIR" -G Ninja --fresh

printf '==> build\n'
"$CMAKE" --build "$BUILDDIR"

printf '==> test\n'
"$CTEST" --test-dir "$BUILDDIR" --output-on-failure

if command -v "$CLANG_FORMAT" >/dev/null 2>&1; then
  printf '==> clang-format --dry-run\n'
  # Only our own sources; vendored dependencies follow their upstream style.
  find src \( -name '*.cc' -o -name '*.h' \) -print0 \
    | xargs -0 "$CLANG_FORMAT" --dry-run --Werror
else
  printf 'clang-format not found; skipping the format check\n' >&2
fi

if command -v shellcheck >/dev/null 2>&1; then
  printf '==> shellcheck\n'
  git ls-files '*.sh' | xargs shellcheck -x
else
  printf 'shellcheck not found; skipping the shell lint check\n' >&2
fi

printf '==> asan/ubsan build and test\n'
# AddressSanitizer plus UndefinedBehaviorSanitizer over the same test suite;
# halt_on_error makes the first finding fail the run.
ASAN_OPTIONS=halt_on_error=1 UBSAN_OPTIONS=halt_on_error=1 \
  "$(dirname "$0")/sanitize-ci.sh"

printf 'local CI passed\n'
