// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_TUI_TIMEZONE_H_
#define PUU_TUI_TIMEZONE_H_

#include <string>
#include <string_view>
#include <vector>

namespace puu::tui {

// Parses the tzdb `zone1970.tab` format and returns the zone names, sorted,
// de-duplicated and with "UTC" first.
//
// Blank lines, comments, lines short of the three mandatory columns and lines
// whose ISO 6709 coordinate or zone name is malformed are dropped: the name is
// joined onto a path later, so it is validated here rather than downstream.
// Split out from the file lookup so it can be tested without a system tzdb.
std::vector<std::string> ParseZoneTab(std::string_view contents);

// Zone names read from the system tzdb on first use, or a built-in fallback
// when no `zone1970.tab` is installed.
const std::vector<std::string>& Timezones();

}  // namespace puu::tui

#endif  // PUU_TUI_TIMEZONE_H_
