// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/passwd.h"

#include <array>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include <fmt/format.h>
#include <openssl/crypto.h>
#include <openssl/evp.h>

#include "base/status.h"
#include "engine/util.h"

namespace puu::engine {
namespace {

constexpr std::string_view kCryptB64 =
    "./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

constexpr size_t kSaltChars = 16;
constexpr size_t kDigestBytes = 64;

using Digest512 = std::array<uint8_t, kDigestBytes>;

std::span<const uint8_t> AsBytes(std::string_view text) {
  return {reinterpret_cast<const uint8_t*>(text.data()), text.size()};
}

class Sha512 {
 public:
  Sha512() : ctx_(EVP_MD_CTX_new()) { Reset(); }

  void Reset() {
    if (ctx_ == nullptr) {
      ok_ = false;
      return;
    }
    ok_ = EVP_DigestInit_ex(ctx_.get(), EVP_sha512(), nullptr) == 1;
  }

  void Update(std::span<const uint8_t> data) {
    if (!ok_ || data.empty()) {
      return;
    }
    ok_ = EVP_DigestUpdate(ctx_.get(), data.data(), data.size()) == 1;
  }

  bool Finish(Digest512& out) {
    if (!ok_) {
      return false;
    }
    unsigned int size = 0;
    ok_ = EVP_DigestFinal_ex(ctx_.get(), out.data(), &size) == 1 &&
          size == out.size();
    const bool finished = ok_;
    Reset();
    return finished;
  }

 private:
  struct Deleter {
    void operator()(EVP_MD_CTX* ctx) const { EVP_MD_CTX_free(ctx); }
  };

  bool ok_ = true;
  std::unique_ptr<EVP_MD_CTX, Deleter> ctx_;
};

std::vector<uint8_t> Tile(const Digest512& digest, size_t length) {
  std::vector<uint8_t> out;
  out.reserve(length);
  while (out.size() + digest.size() <= length) {
    out.insert(out.end(), digest.begin(), digest.end());
  }
  out.insert(out.end(), digest.begin(),
             digest.begin() + static_cast<std::ptrdiff_t>(length - out.size()));
  return out;
}

std::string EncodeDigest(const Digest512& digest) {
  constexpr std::array<std::array<uint8_t, 3>, 21> kGroups = {{
      {0, 21, 42},  {22, 43, 1},  {44, 2, 23},  {3, 24, 45},  {25, 46, 4},
      {47, 5, 26},  {6, 27, 48},  {28, 49, 7},  {50, 8, 29},  {9, 30, 51},
      {31, 52, 10}, {53, 11, 32}, {12, 33, 54}, {34, 55, 13}, {56, 14, 35},
      {15, 36, 57}, {37, 58, 16}, {59, 17, 38}, {18, 39, 60}, {40, 61, 19},
      {62, 20, 41},
  }};

  std::string out;
  out.reserve(86);
  const auto emit = [&out](uint32_t word, size_t count) {
    for (size_t index = 0; index < count; ++index) {
      out.push_back(kCryptB64[word & 0x3f]);
      word >>= 6;
    }
  };
  for (const std::array<uint8_t, 3>& group : kGroups) {
    emit((static_cast<uint32_t>(digest[group[0]]) << 16) |
             (static_cast<uint32_t>(digest[group[1]]) << 8) |
             static_cast<uint32_t>(digest[group[2]]),
         4);
  }
  emit(digest[63], 2);
  return out;
}

StatusOr<std::string> Sha512Crypt(std::string_view password,
                                  std::string_view salt) {
  const std::span<const uint8_t> key = AsBytes(password);
  const std::span<const uint8_t> salt_bytes = AsBytes(salt);

  Sha512 hash;
  Digest512 alternate = {};
  Digest512 result = {};

  hash.Update(key);
  hash.Update(salt_bytes);
  hash.Update(key);
  bool ok = hash.Finish(alternate);

  hash.Update(key);
  hash.Update(salt_bytes);
  size_t remaining = key.size();
  for (; remaining > kDigestBytes; remaining -= kDigestBytes) {
    hash.Update(alternate);
  }
  hash.Update(std::span<const uint8_t>(alternate).first(remaining));
  for (size_t bits = key.size(); bits > 0; bits >>= 1) {
    hash.Update((bits & 1) != 0 ? std::span<const uint8_t>(alternate) : key);
  }
  ok = hash.Finish(result) && ok;

  for (size_t index = 0; index < key.size(); ++index) {
    hash.Update(key);
  }
  ok = hash.Finish(alternate) && ok;
  std::vector<uint8_t> key_sequence = Tile(alternate, key.size());

  const size_t salt_repeats = 16 + result[0];
  for (size_t index = 0; index < salt_repeats; ++index) {
    hash.Update(salt_bytes);
  }
  ok = hash.Finish(alternate) && ok;
  std::vector<uint8_t> salt_sequence = Tile(alternate, salt.size());

  for (int round = 0; round < kSha512CryptRounds; ++round) {
    const bool odd = (round & 1) != 0;
    hash.Update(odd ? std::span<const uint8_t>(key_sequence)
                    : std::span<const uint8_t>(result));
    if (round % 3 != 0) {
      hash.Update(salt_sequence);
    }
    if (round % 7 != 0) {
      hash.Update(key_sequence);
    }
    hash.Update(odd ? std::span<const uint8_t>(result)
                    : std::span<const uint8_t>(key_sequence));
    ok = hash.Finish(result) && ok;
  }

  std::string encoded = EncodeDigest(result);

  OPENSSL_cleanse(alternate.data(), alternate.size());
  OPENSSL_cleanse(result.data(), result.size());
  OPENSSL_cleanse(key_sequence.data(), key_sequence.size());
  OPENSSL_cleanse(salt_sequence.data(), salt_sequence.size());

  if (!ok) {
    return InternalError("SHA-512 digest failed");
  }
  return encoded;
}

}  // namespace

StatusOr<std::string> HashPassword(std::string_view password) {
  std::array<uint8_t, kSaltChars> random = {};
  PUU_RETURN_IF_ERROR(RandomBytes(random));

  std::string salt;
  salt.reserve(kSaltChars);
  for (const uint8_t byte : random) {
    salt.push_back(kCryptB64[byte & 0x3f]);
  }

  PUU_ASSIGN_OR_RETURN(const std::string encoded, Sha512Crypt(password, salt));
  return fmt::format("$6${}${}", salt, encoded);
}

}  // namespace puu::engine
