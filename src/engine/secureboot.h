// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_SECUREBOOT_H_
#define PUU_ENGINE_SECUREBOOT_H_

#include <cstdint>
#include <filesystem>
#include <string_view>
#include <vector>

#include "base/status.h"
#include "engine/firmware.h"

namespace puu::engine {

enum class KeyKind : std::uint8_t {
  // Platform Key: the root of trust. Enrolling it leaves Setup Mode.
  kPlatform,
  // Key Exchange Key: signs updates to the signature databases.
  kKeyExchange,
  // Signature database: the keys allowed to sign bootable images.
  kDatabase,
};

std::string_view KeyKindName(KeyKind kind);

struct KeyPair {
  std::filesystem::path key_path;
  std::filesystem::path certificate_path;
};

// The key store written under the target root, compatible with sbctl's layout.
class KeyStore {
 public:
  explicit KeyStore(std::filesystem::path root);

  // Loads the existing keys, or generates a fresh set when any is missing.
  //
  // Generated private keys are written with mode 0600. An existing complete set
  // is reused so that reinstalling does not invalidate an already-enrolled
  // platform key.
  Status EnsureKeys();

  [[nodiscard]] StatusOr<KeyPair> Get(KeyKind kind) const;

  [[nodiscard]] StatusOr<EfiGuid> OwnerGuid() const;

 private:
  std::filesystem::path root_;
};

// Signs `binary` in place with an Authenticode SHA-256 signature from the
// database key.
Status SignPeBinary(const std::filesystem::path& binary, const KeyPair& key);

StatusOr<bool> VerifyPeBinary(const std::filesystem::path& binary,
                              const KeyPair& key);

// Every *.efi file under `esp`, sorted, so signing order is reproducible.
StatusOr<std::vector<std::filesystem::path>> TargetEfiBinaries(
    const std::filesystem::path& esp);

// Enrols the key hierarchy into firmware.
//
// Order matters and is not configurable: db and KEK must be in place before the
// platform key, because enrolling PK closes Setup Mode and refuses any further
// unsigned update.
Status EnrollKeys(const KeyStore& store);

}  // namespace puu::engine

#endif  // PUU_ENGINE_SECUREBOOT_H_
