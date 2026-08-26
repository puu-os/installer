#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

# Builds and tests with AddressSanitizer and UndefinedBehaviorSanitizer in a
# dedicated build directory. Invoked by ci.sh; also usable directly.

set -euo pipefail

MESON="${MESON:-meson}"
BUILDDIR="${SANITIZE_BUILDDIR:-build-sanitize}"

# GCC 14 rejects abseil's hash-table internals under -fsanitize=undefined
# (function-pointer comparison stops being a constant expression), so build
# that subproject unsanitized; our code is still fully instrumented.
NO_ABLSAN='-Dabseil-cpp:b_sanitize=none'
if [[ -d "$BUILDDIR" ]]; then
  "$MESON" setup --reconfigure "$BUILDDIR" -Db_sanitize=address,undefined \
    -Dtests=enabled "$NO_ABLSAN" >/dev/null
else
  "$MESON" setup "$BUILDDIR" -Db_sanitize=address,undefined \
    -Dtests=enabled "$NO_ABLSAN" >/dev/null
fi

ninja -C "$BUILDDIR"
"$MESON" test -C "$BUILDDIR" --print-errorlogs
