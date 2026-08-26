// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/progress.h"

#include <sys/statvfs.h>

#include <algorithm>
#include <charconv>
#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <optional>
#include <string_view>
#include <system_error>
#include <utility>

namespace puu::engine {
namespace {

constexpr std::string_view kProgressMarker = "PUU_PROGRESS";

std::optional<double> ParseMarker(std::string_view line) {
  const size_t marker = line.find(kProgressMarker);
  if (marker == std::string_view::npos) {
    return std::nullopt;
  }

  std::string_view rest = line.substr(marker + kProgressMarker.size());
  const size_t start = rest.find_first_not_of(" \t");
  if (start == std::string_view::npos) {
    return std::nullopt;
  }
  rest.remove_prefix(start);
  rest = rest.substr(0, rest.find_first_of(" \t"));

  const size_t slash = rest.find('/');
  if (slash == std::string_view::npos) {
    return std::nullopt;
  }

  const auto number = [](std::string_view text) -> std::optional<double> {
    double value = 0.0;
    const auto [end, error] =
        std::from_chars(text.data(), text.data() + text.size(), value);
    if (error != std::errc() || end != text.data() + text.size()) {
      return std::nullopt;
    }
    return value;
  };

  const std::optional<double> done = number(rest.substr(0, slash));
  const std::optional<double> total = number(rest.substr(slash + 1));
  if (!done || !total || *total <= 0.0) {
    return std::nullopt;
  }
  return std::clamp(*done / *total, 0.0, ProgressSource::kMaxFraction);
}

}  // namespace

DiskWriteProgress::DiskWriteProgress(std::filesystem::path mount_point,
                                     uint64_t expected_bytes)
    : mount_point_(std::move(mount_point)), expected_bytes_(expected_bytes) {}

std::optional<uint64_t> DiskWriteProgress::AvailableBytes() const {
  struct statvfs stat = {};
  if (::statvfs(mount_point_.c_str(), &stat) != 0) {
    return std::nullopt;
  }
  return static_cast<uint64_t>(stat.f_bavail) *
         static_cast<uint64_t>(stat.f_frsize);
}

std::optional<double> DiskWriteProgress::Fraction() {
  if (expected_bytes_ == 0) {
    return std::nullopt;
  }
  const std::optional<uint64_t> available = AvailableBytes();
  if (!available) {
    return std::nullopt;
  }

  if (!baseline_available_) {
    baseline_available_ = available;
  }

  const uint64_t written =
      *baseline_available_ > *available ? *baseline_available_ - *available : 0;
  const double fraction = std::clamp(
      static_cast<double>(written) / static_cast<double>(expected_bytes_), 0.0,
      kMaxFraction);

  peak_fraction_ = std::max(peak_fraction_, fraction);
  return peak_fraction_;
}

std::optional<double> MarkerProgress::Fraction() { return fraction_; }

void MarkerProgress::ObserveLine(std::string_view line) {
  if (const std::optional<double> parsed = ParseMarker(line)) {
    fraction_ = parsed;
  }
}

}  // namespace puu::engine
