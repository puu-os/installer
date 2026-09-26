// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_PASSWD_H_
#define PUU_ENGINE_PASSWD_H_

#include <string>
#include <string_view>

#include "base/status.h"

namespace puu::engine {

// Number of hashing rounds recorded in the crypt string. Matches the glibc
// default so the hash stays verifiable by the installed system.
inline constexpr int kSha512CryptRounds = 5000;

// Produces a "$6$" SHA-512 crypt hash of `password` with a fresh random salt.
//
// The result is safe to write to disk; the plaintext is not retained.
StatusOr<std::string> HashPassword(std::string_view password);
StatusOr<std::string> Sha512Crypt(std::string_view password,
                                  std::string_view salt);

}  // namespace puu::engine

#endif  // PUU_ENGINE_PASSWD_H_
