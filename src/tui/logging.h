// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_TUI_LOGGING_H_
#define PUU_TUI_LOGGING_H_

#include <string>
#include <string_view>
#include <vector>

#include "base/status.h"

namespace puu::tui {

// Truncates to at most `max_bytes` without splitting a UTF-8 sequence.
std::string TruncateUtf8(std::string_view text, size_t max_bytes);

// Creates /run/puu-installer/install.log and points spdlog at it, plus an
// in-memory sink feeding the installation screen. Call once, before the UI
// starts. The log holds passwords in neither sink, but it does hold device
// paths, so both the directory and the file are owner-only.
Status Init();

// A copy of the most recent log lines, oldest first, capped at the ring size.
// Safe to call from the UI thread while the installation worker is logging.
std::vector<std::string> SnapshotLogLines();

}  // namespace puu::tui

#endif  // PUU_TUI_LOGGING_H_
