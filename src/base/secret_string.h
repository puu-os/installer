// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_BASE_SECRET_STRING_H_
#define PUU_BASE_SECRET_STRING_H_

#include <cstddef>
#include <string>
#include <string_view>

namespace puu {

// A string whose buffer is wiped before release. Used for the user password and
// the k3s cluster token, which must not survive in freed heap pages.
//
// The wipe is best effort against a debugger, but it does defeat accidental
// disclosure through core dumps and reused allocations. Callers must not copy
// the contents into an unprotected std::string.
class SecretString {
 public:
  SecretString() = default;
  explicit SecretString(std::string_view text);

  SecretString(const SecretString&) = delete;
  SecretString& operator=(const SecretString&) = delete;
  SecretString(SecretString&& other);
  SecretString& operator=(SecretString&& other);

  ~SecretString();

  // Borrowed view; invalidated by any mutation or by destruction.
  std::string_view value() const { return value_; }
  bool empty() const { return value_.empty(); }
  size_t size() const { return value_.size(); }

  // Explicit because secrets must never be copied accidentally.
  SecretString Clone() const;

  void Assign(std::string_view text);

  void Clear();

 private:
  void Wipe();

  std::string value_;
};

}  // namespace puu

#endif  // PUU_BASE_SECRET_STRING_H_
