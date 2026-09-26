// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "base/secret_string.h"

#include <string>
#include <string_view>
#include <utility>

#include <openssl/crypto.h>

namespace puu {

void WipeString(std::string* value) {
  if (value == nullptr) {
    return;
  }
  if (!value->empty()) {
    OPENSSL_cleanse(value->data(), value->size());
  }
  const size_t cap = value->capacity();
  if (cap > 0) {
    value->resize(cap, '\0');
    OPENSSL_cleanse(value->data(), cap);
  }
  value->clear();
}

SecretString::SecretString(std::string_view text) : value_(text) {}

SecretString::~SecretString() { Wipe(); }

SecretString::SecretString(SecretString&& other) noexcept
    : value_(std::move(other.value_)) {
  other.Wipe();
}

SecretString& SecretString::operator=(SecretString&& other) noexcept {
  if (this != &other) {
    Wipe();
    value_ = std::move(other.value_);
    other.Wipe();
  }
  return *this;
}

SecretString SecretString::Clone() const { return SecretString(value_); }

void SecretString::Assign(std::string_view text) {
  Wipe();
  value_.assign(text.data(), text.size());
}

void SecretString::Clear() { Wipe(); }

void SecretString::Wipe() { WipeString(&value_); }

bool operator==(const SecretString& lhs, const SecretString& rhs) noexcept {
  if (lhs.Size() != rhs.Size()) {
    return false;
  }
  return CRYPTO_memcmp(lhs.Value().data(), rhs.Value().data(), lhs.Size()) == 0;
}

bool operator==(const SecretString& lhs, std::string_view rhs) noexcept {
  if (lhs.Size() != rhs.size()) {
    return false;
  }
  return CRYPTO_memcmp(lhs.Value().data(), rhs.data(), lhs.Size()) == 0;
}

bool operator==(std::string_view lhs, const SecretString& rhs) noexcept {
  return rhs == lhs;
}

bool operator!=(const SecretString& lhs, const SecretString& rhs) noexcept {
  return !(lhs == rhs);
}

bool operator!=(const SecretString& lhs, std::string_view rhs) noexcept {
  return !(lhs == rhs);
}

bool operator!=(std::string_view lhs, const SecretString& rhs) noexcept {
  return !(lhs == rhs);
}

}  // namespace puu
