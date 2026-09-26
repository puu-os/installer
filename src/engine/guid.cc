// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/guid.h"

#include <algorithm>
#include <array>
#include <cstdint>
#include <optional>
#include <string>
#include <string_view>

#include <fmt/format.h>

#include "base/status.h"
#include "engine/le_bytes.h"
#include "engine/util.h"

namespace puu::engine {
namespace {

constexpr std::optional<uint8_t> HexDigitValue(char c) {
  if (c >= '0' && c <= '9') {
    return static_cast<uint8_t>(c - '0');
  }
  if (c >= 'a' && c <= 'f') {
    return static_cast<uint8_t>(c - 'a' + 10);
  }
  if (c >= 'A' && c <= 'F') {
    return static_cast<uint8_t>(c - 'A' + 10);
  }
  return std::nullopt;
}

}  // namespace

std::string EfiGuid::ToString() const {
  const auto data1 = ReadLe<uint32_t>(bytes_, 0);
  const auto data2 = ReadLe<uint16_t>(bytes_, 4);
  const auto data3 = ReadLe<uint16_t>(bytes_, 6);
  return fmt::format(
      "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
      data1, data2, data3, bytes_[8], bytes_[9], bytes_[10], bytes_[11],
      bytes_[12], bytes_[13], bytes_[14], bytes_[15]);
}

StatusOr<EfiGuid> EfiGuid::Parse(std::string_view text) {
  std::array<uint8_t, 16> big_endian{};
  size_t nibble = 0;
  for (const char c : text) {
    if (c == '-') {
      continue;
    }
    const auto value = HexDigitValue(c);
    if (!value.has_value()) {
      return InvalidArgumentError("invalid character in UUID '{}'", text);
    }
    if (nibble >= 32) {
      return InvalidArgumentError("UUID '{}' is too long", text);
    }
    if (nibble % 2 == 0) {
      big_endian[nibble / 2] = static_cast<uint8_t>(*value << 4U);
    } else {
      big_endian[nibble / 2] =
          static_cast<uint8_t>(big_endian[nibble / 2] | *value);
    }
    ++nibble;
  }
  if (nibble != 32) {
    return InvalidArgumentError("UUID '{}' is too short", text);
  }

  std::array<uint8_t, 16> mixed{};
  mixed[0] = big_endian[3];
  mixed[1] = big_endian[2];
  mixed[2] = big_endian[1];
  mixed[3] = big_endian[0];
  mixed[4] = big_endian[5];
  mixed[5] = big_endian[4];
  mixed[6] = big_endian[7];
  mixed[7] = big_endian[6];
  std::copy(big_endian.begin() + 8, big_endian.end(), mixed.begin() + 8);
  return EfiGuid(mixed);
}

StatusOr<EfiGuid> EfiGuid::GenerateRandom() {
  std::array<uint8_t, 16> bytes{};
  PUU_RETURN_IF_ERROR(RandomBytes(bytes));
  // In mixed-endian format:
  // Data3 high byte is bytes[7]: set version 4 in top nibble (0x40..0x4f).
  bytes[7] = static_cast<uint8_t>((bytes[7] & 0x0fU) | 0x40U);
  // Data4 first byte is bytes[8]: set RFC 4122 variant 1 in top 2 bits (0x80..0xbf).
  bytes[8] = static_cast<uint8_t>((bytes[8] & 0x3fU) | 0x80U);
  return EfiGuid(bytes);
}

}  // namespace puu::engine
