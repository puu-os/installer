// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_BASE_STATUS_H_
#define PUU_BASE_STATUS_H_

#include <string_view>
#include <utility>

#include <absl/status/status.h>
#include <absl/status/statusor.h>
#include <fmt/base.h>
#include <fmt/format.h>

namespace puu {

using absl::Status;
using absl::StatusOr;

template <typename... Args>
Status InvalidArgumentError(fmt::format_string<Args...> format,
                            Args&&... args) {
  return absl::InvalidArgumentError(
      fmt::format(format, std::forward<Args>(args)...));
}

template <typename... Args>
Status FailedPreconditionError(fmt::format_string<Args...> format,
                               Args&&... args) {
  return absl::FailedPreconditionError(
      fmt::format(format, std::forward<Args>(args)...));
}

template <typename... Args>
Status InternalError(fmt::format_string<Args...> format, Args&&... args) {
  return absl::InternalError(fmt::format(format, std::forward<Args>(args)...));
}

template <typename... Args>
Status NotFoundError(fmt::format_string<Args...> format, Args&&... args) {
  return absl::NotFoundError(fmt::format(format, std::forward<Args>(args)...));
}

// Wraps `errno` from a failed syscall into a status carrying the operation that
// failed. `error_number` must be captured immediately after the failing call.
Status ErrnoToStatus(int error_number, std::string_view operation);

// Returns `status` with `context` prepended, preserving the error code. Use it
// where the failure gains meaning, not at every propagation point.
Status WithContext(const Status& status, std::string_view context);

}  // namespace puu

#define PUU_RETURN_IF_ERROR(expr)                                    \
  do {                                                               \
    ::puu::Status PUU_STATUS_CONCAT(puu_status_, __LINE__) = (expr); \
    if (!PUU_STATUS_CONCAT(puu_status_, __LINE__).ok())              \
      return PUU_STATUS_CONCAT(puu_status_, __LINE__);               \
  } while (false)

#define PUU_STATUS_CONCAT_INNER(a, b) a##b
#define PUU_STATUS_CONCAT(a, b) PUU_STATUS_CONCAT_INNER(a, b)

// Assigns the value of a StatusOr to `lhs`, propagating failure. `lhs` may
// include a declaration, for example: `PUU_ASSIGN_OR_RETURN(auto fd, Open(p))`.
#define PUU_ASSIGN_OR_RETURN(lhs, expr)                      \
  lhs = __extension__({                                      \
    auto PUU_STATUS_CONCAT(puu_or_, __LINE__) = (expr);      \
    if (!PUU_STATUS_CONCAT(puu_or_, __LINE__).ok())          \
      return PUU_STATUS_CONCAT(puu_or_, __LINE__).status();  \
    std::move(PUU_STATUS_CONCAT(puu_or_, __LINE__)).value(); \
  })

#endif  // PUU_BASE_STATUS_H_
