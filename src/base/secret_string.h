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
  SecretString(SecretString&& other) noexcept;
  SecretString& operator=(SecretString&& other) noexcept;

  ~SecretString();

  // Borrowed view; invalidated by any mutation or by destruction.
  [[nodiscard]] std::string_view Value() const { return value_; }
  [[nodiscard]] bool Empty() const { return value_.empty(); }
  [[nodiscard]] size_t Size() const { return value_.size(); }

  // Explicit because secrets must never be copied accidentally.
  [[nodiscard]] SecretString Clone() const;

  void Assign(std::string_view text);

  void Clear();

 private:
  void Wipe();

  std::string value_;
};

// Cleanses and clears a std::string holding sensitive data.
void WipeString(std::string* value);

// Constant-time equality comparison to protect against timing attacks.
[[nodiscard]] bool operator==(const SecretString& lhs,
                              const SecretString& rhs) noexcept;
[[nodiscard]] bool operator==(const SecretString& lhs,
                              std::string_view rhs) noexcept;
[[nodiscard]] bool operator==(std::string_view lhs,
                              const SecretString& rhs) noexcept;
[[nodiscard]] bool operator!=(const SecretString& lhs,
                              const SecretString& rhs) noexcept;
[[nodiscard]] bool operator!=(const SecretString& lhs,
                              std::string_view rhs) noexcept;
[[nodiscard]] bool operator!=(std::string_view lhs,
                              const SecretString& rhs) noexcept;

}  // namespace puu

#endif  // PUU_BASE_SECRET_STRING_H_
