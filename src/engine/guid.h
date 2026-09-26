// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_GUID_H_
#define PUU_ENGINE_GUID_H_

#include <array>
#include <cstdint>
#include <string>
#include <string_view>

#include "base/status.h"
#include "engine/le_bytes.h"

namespace puu::engine {

// An EFI / GPT GUID, stored in its 16-byte mixed-endian form.
class EfiGuid {
 public:
  constexpr EfiGuid() = default;
  explicit constexpr EfiGuid(std::array<uint8_t, 16> bytes) : bytes_(bytes) {}

  // Constructs an EFI GUID from standard UEFI Data1, Data2, Data3, Data4 parts.
  static constexpr EfiGuid FromParts(uint32_t data1, uint16_t data2,
                                     uint16_t data3,
                                     const std::array<uint8_t, 8>& data4) {
    std::array<uint8_t, 16> bytes{};
    PutLe<uint32_t>(bytes, 0, data1);
    PutLe<uint16_t>(bytes, 4, data2);
    PutLe<uint16_t>(bytes, 6, data3);
    for (size_t i = 0; i < 8; ++i) {
      bytes[8 + i] = data4[i];
    }
    return EfiGuid(bytes);
  }

  // The EFI global variable namespace, 8be4df61-93ca-11d2-aa0d-00e098032b8c.
  static constexpr EfiGuid Global() {
    return FromParts(0x8be4df61, 0x93ca, 0x11d2,
                     {0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c});
  }

  // The image security database namespace, used for "db" and "dbx".
  static constexpr EfiGuid ImageSecurityDatabase() {
    return FromParts(0xd719b2cb, 0x3d3a, 0x4596,
                     {0xa3, 0xbc, 0xda, 0xd0, 0x0e, 0x67, 0x65, 0x6f});
  }

  // Standard EFI System Partition (ESP) partition type GUID:
  // c12a7328-f81f-11d2-ba4b-00a0c93ec93b.
  static constexpr EfiGuid EspType() {
    return FromParts(0xc12a7328, 0xf81f, 0x11d2,
                     {0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b});
  }

  // Standard Linux filesystem partition type GUID:
  // 0fc63daf-8483-4772-8e79-3d69d8477de4.
  static constexpr EfiGuid LinuxFilesystemType() {
    return FromParts(0x0fc63daf, 0x8483, 0x4772,
                     {0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4});
  }
  // Standard EFI X.509 certificate signature list type GUID:
  // a5c059a1-94e4-4aa7-87b5-ab155c2bf072.
  static constexpr EfiGuid CertX509() {
    return FromParts(0xa5c059a1, 0x94e4, 0x4aa7,
                     {0x87, 0xb5, 0xab, 0x15, 0x5c, 0x2b, 0xf0, 0x72});
  }

  // Standard EFI PKCS#7 certificate signature type GUID:
  // 4aafd29d-68df-49ee-8aa9-347d375665a7.
  static constexpr EfiGuid CertPkcs7() {
    return FromParts(0x4aafd29d, 0x68df, 0x49ee,
                     {0x8a, 0xa9, 0x34, 0x7d, 0x37, 0x56, 0x65, 0xa7});
  }

  // Parses a 32- or 36-character UUID string (with or without hyphens)
  // into mixed-endian EFI format.
  static StatusOr<EfiGuid> Parse(std::string_view text);

  // Generates a random version 4 (variant 1) EFI GUID.
  static StatusOr<EfiGuid> GenerateRandom();

  [[nodiscard]] constexpr const std::array<uint8_t, 16>& Bytes() const {
    return bytes_;
  }
  [[nodiscard]] std::string ToString() const;

  friend bool operator==(const EfiGuid& lhs, const EfiGuid& rhs) = default;

 private:
  std::array<uint8_t, 16> bytes_{};
};

}  // namespace puu::engine

#endif  // PUU_ENGINE_GUID_H_
