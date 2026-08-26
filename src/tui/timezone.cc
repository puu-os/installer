// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "tui/timezone.h"

#include <algorithm>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <string>
#include <string_view>
#include <vector>

#include <absl/base/no_destructor.h>
#include <absl/strings/ascii.h>
#include <absl/strings/str_split.h>

#include "config.h"
#include "tui/zone1970_tab.inc"

namespace puu::tui {
namespace {

constexpr std::string_view kZoneTabPath = "/usr/share/zoneinfo/zone1970.tab";

constexpr std::string_view kUtc = "UTC";

std::vector<std::string> ReadZoneTab() {
  std::ifstream file{std::filesystem::path{kZoneTabPath}};
  if (file) {
    std::string const contents{std::istreambuf_iterator<char>(file),
                               std::istreambuf_iterator<char>()};

    if (!file.bad()) {
      return ParseZoneTab(contents);
    }
  }
  return ParseZoneTab(kEmbeddedZone1970Tab);
}

}  // namespace

std::vector<std::string> ParseZoneTab(std::string_view contents) {
  std::vector<std::string> zones;
  for (std::string_view line : absl::StrSplit(contents, '\n')) {
    line = absl::StripTrailingAsciiWhitespace(line);
    if (line.empty() || line.front() == '#') {
      continue;
    }

    const std::vector<std::string_view> columns = absl::StrSplit(line, '\t');

    if (columns.size() < 3) {
      continue;
    }
    if (columns[0].empty()) {
      continue;
    }
    if (!Timezone::Validate(columns[2]).ok()) {
      continue;
    }
    zones.emplace_back(columns[2]);
  }

  std::ranges::sort(zones);
  zones.erase(std::ranges::unique(zones).begin(), zones.end());

  zones.erase(std::ranges::remove(zones, kUtc).begin(), zones.end());
  zones.insert(zones.begin(), std::string(kUtc));
  return zones;
}

const std::vector<std::string>& Timezones() {
  static const absl::NoDestructor<std::vector<std::string>> zones(
      ReadZoneTab());
  return *zones;
}

}  // namespace puu::tui
