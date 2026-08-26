// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "base/secret_string.h"

#include <utility>

#include <openssl/crypto.h>

namespace puu {

SecretString::SecretString(std::string_view text) : value_(text) {}

SecretString::~SecretString() { Wipe(); }

SecretString::SecretString(SecretString&& other) : value_(other.value_) {
  other.Clear();
}

SecretString& SecretString::operator=(SecretString&& other) {
  if (this != &other) {
    Clear();
    value_ = other.value_;
    other.Clear();
  }
  return *this;
}

SecretString SecretString::Clone() const { return SecretString(value_); }

void SecretString::Assign(std::string_view text) {
  Wipe();
  value_.assign(text.data(), text.size());
}

void SecretString::Clear() {
  Wipe();
  value_.clear();
}

void SecretString::Wipe() {
  if (!value_.empty()) {
    OPENSSL_cleanse(value_.data(), value_.size());
  }
}

}  // namespace puu
