#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

# Builds and tests with AddressSanitizer and UndefinedBehaviorSanitizer in a
# dedicated build directory. Invoked by ci.sh; also usable directly.

set -euo pipefail

CMAKE="${CMAKE:-cmake}"
CTEST="${CTEST:-ctest}"
BUILDDIR="${SANITIZE_BUILDDIR:-build-sanitize}"

# PUU_SANITIZE instruments only our own code; dependencies stay unsanitized.
# GCC 14 rejects Abseil's hash-table internals under -fsanitize=undefined
# (function-pointer comparison stops being a constant expression), so using
# the uninstrumented system package also keeps those internals out of reach.
"$CMAKE" -S . -B "$BUILDDIR" -G Ninja -DPUU_SANITIZE=address,undefined \
  >/dev/null

"$CMAKE" --build "$BUILDDIR"
"$CTEST" --test-dir "$BUILDDIR" --output-on-failure
