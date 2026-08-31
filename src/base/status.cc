// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "base/status.h"

#include <string_view>

#include <absl/status/status.h>
#include <absl/strings/cord.h>
#include <fmt/format.h>

namespace puu {

Status ErrnoToStatus(int error_number, std::string_view operation) {
  return absl::ErrnoToStatus(
      error_number, absl::string_view(operation.data(), operation.size()));
}

Status WithContext(const Status& status, std::string_view context) {
  if (status.ok()) {
    return status;
  }
  Status result(status.code(),
                fmt::format("{}: {}", context, status.message()));
  status.ForEachPayload(
      [&result](absl::string_view type_url, const absl::Cord& payload) {
        result.SetPayload(type_url, payload);
      });
  return result;
}

}  // namespace puu
