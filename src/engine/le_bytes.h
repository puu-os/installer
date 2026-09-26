// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026
//
// The one little-endian codec. Every disk and firmware structure this
// installer touches is little-endian.

#ifndef PUU_ENGINE_LE_BYTES_H_
#define PUU_ENGINE_LE_BYTES_H_

#include <concepts>
#include <cstddef>
#include <cstdint>
#include <span>
#include <vector>

namespace puu::engine {

// Appends `value` to `out` in little-endian byte order.
template <typename T>
  requires std::unsigned_integral<T>
void AppendLe(std::vector<uint8_t>& out, T value) {
  for (size_t index = 0; index < sizeof(T); ++index) {
    out.push_back(static_cast<uint8_t>(value >> (8 * index)));
  }
}

// Writes `value` at `buffer[offset]` in little-endian byte order. The caller
// owns the bounds check.
template <typename T>
  requires std::unsigned_integral<T>
constexpr void PutLe(std::span<uint8_t> buffer, size_t offset, T value) {
  for (size_t index = 0; index < sizeof(T); ++index) {
    buffer[offset + index] = static_cast<uint8_t>(value >> (8 * index));
  }
}

// Reads the little-endian `T` at `buffer[offset]`. The caller owns the bounds
// check.
template <typename T>
  requires std::unsigned_integral<T>
constexpr T ReadLe(std::span<const uint8_t> buffer, size_t offset) {
  T value = 0;
  for (size_t index = 0; index < sizeof(T); ++index) {
    value = static_cast<T>(
        value | (static_cast<T>(buffer[offset + index]) << (8 * index)));
  }
  return value;
}

template <typename T>
  requires std::unsigned_integral<T>
constexpr bool LeFits(std::span<const uint8_t> buffer, size_t offset) {
  return offset <= buffer.size() && buffer.size() - offset >= sizeof(T);
}

}  // namespace puu::engine

#endif  // PUU_ENGINE_LE_BYTES_H_
