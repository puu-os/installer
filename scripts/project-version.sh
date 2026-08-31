# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

read_project_version() {
  sed -n 's/^[[:space:]]*VERSION[[:space:]]*\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' \
    "${1:-CMakeLists.txt}" | head -1
}
