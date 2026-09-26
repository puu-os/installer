// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_UTIL_H_
#define PUU_ENGINE_UTIL_H_

#include <fcntl.h>
#include <sys/types.h>

#include <array>
#include <chrono>
#include <cstdint>
#include <filesystem>
#include <functional>
#include <optional>
#include <set>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <vector>

#include "base/secret_string.h"
#include "base/status.h"

namespace puu::engine {

Status FilesystemError(const std::error_code& error,
                       std::string_view operation);
Status CreateDirectories(const std::filesystem::path& path);
bool IsDirectory(const std::filesystem::path& path);
bool IsRegularFile(const std::filesystem::path& path);
bool Exists(const std::filesystem::path& path);
std::filesystem::path Canonical(const std::filesystem::path& path);
Status CopyFile(const std::filesystem::path& source,
                const std::filesystem::path& destination);
Status Rename(const std::filesystem::path& source,
              const std::filesystem::path& destination);
StatusOr<std::filesystem::path> ReadSymlink(const std::filesystem::path& path);
Status CreateSymlink(const std::filesystem::path& target,
                     const std::filesystem::path& link_path);
Status RemoveFile(const std::filesystem::path& path);
Status RemoveTree(const std::filesystem::path& path);

// Writes, fsyncs and chmods a file. `extra_flags` may add O_NOFOLLOW.
Status WriteBytes(const std::filesystem::path& path,
                  std::span<const uint8_t> data, mode_t mode,
                  int extra_flags = 0);
Status WriteFile(const std::filesystem::path& path, std::string_view content,
                 mode_t mode, int extra_flags = 0);
Status ReplaceFileBytes(const std::filesystem::path& path,
                        std::span<const uint8_t> data);
Status ReplaceFile(const std::filesystem::path& path, std::string_view content);

// Device nodes backing mounts in /proc/self/mountinfo.
StatusOr<std::set<std::filesystem::path>> MountedSources();

// Fills `buffer` with cryptographically secure random bytes.
Status RandomBytes(std::span<uint8_t> buffer);

// Flushes all filesystem buffers to disk. Called before reboot and after
// staging boot files.
void SyncAll();

// True when the effective user id is 0.
bool RunningAsRoot();

// True when the process is running inside a podman container, which is the only
// situation where a missing local source image is acceptable.
bool RunningInContainer();

// Polls `predicate` every `interval` until it returns true or `timeout`
// elapses. Returns DeadlineExceeded on timeout.
Status WaitUntil(const std::function<bool()>& predicate,
                 std::chrono::milliseconds timeout,
                 std::chrono::milliseconds interval);

// Formats a byte count with binary units, one decimal place, for example
// "465.8 GiB".
std::string HumanSize(uint64_t bytes);

// Reduces a raw command output line for display: keeps only the text after the
// last carriage return, strips CSI escape sequences and drops control
// characters.
std::string CleanLogLine(std::string_view line);

// The user's preferred shell from $SHELL, or /bin/sh.
std::filesystem::path DefaultShell();

// The first executable match for `name` on $PATH, or nullopt. A `name`
// containing a slash is checked directly instead of searched.
std::optional<std::filesystem::path> FindInPath(std::string_view name);

// Reads the first line of `path`, without the line terminator.
StatusOr<std::string> ReadFirstLine(const std::filesystem::path& path);
// Reads the first line of `path` into a SecretString, wiping file buffers.
StatusOr<SecretString> ReadFirstSecretLine(const std::filesystem::path& path);

StatusOr<std::vector<uint8_t>> ReadFileBytes(const std::filesystem::path& path);
StatusOr<std::string> ReadFile(const std::filesystem::path& path);

// `text` without leading and trailing ASCII whitespace.
std::string_view Trim(std::string_view text);

// SHA-256 of `data`, raw and as lowercase hex.
StatusOr<std::array<uint8_t, 32>> Sha256(std::span<const uint8_t> data);
StatusOr<std::string> Sha256Hex(std::span<const uint8_t> data);

// Owns a POSIX file descriptor and closes it on destruction.
class UniqueFd {
 public:
  UniqueFd() = default;
  explicit UniqueFd(int fd);
  ~UniqueFd();

  UniqueFd(const UniqueFd&) = delete;
  UniqueFd& operator=(const UniqueFd&) = delete;
  UniqueFd(UniqueFd&& other) noexcept;
  UniqueFd& operator=(UniqueFd&& other) noexcept;

  [[nodiscard]] int Get() const { return fd_; }
  [[nodiscard]] explicit operator bool() const noexcept { return fd_ >= 0; }
  void Reset(int fd = -1) noexcept;
  [[nodiscard]] int Release() noexcept;

 private:
  int fd_ = -1;
};

// Owns the read and write ends of an anonymous pipe.
struct Pipe {
  UniqueFd read;
  UniqueFd write;
};

// Creates an anonymous unidirectional data channel.
StatusOr<Pipe> CreatePipe(int flags = O_CLOEXEC);

// A parsed view of /proc/self/mountinfo.
class Mounts {
 public:
  static StatusOr<Mounts> Read();

  // True when `path` is itself a mount point. Paths are canonicalised before
  // comparison.
  [[nodiscard]] bool IsMountPoint(const std::filesystem::path& path) const;

  // Mount points below `root`, deepest first, so they can be unmounted in
  // order.
  [[nodiscard]] std::vector<std::filesystem::path> Below(
      const std::filesystem::path& root) const;

  // All mount points, in mountinfo order.
  [[nodiscard]] const std::vector<std::filesystem::path>& MountPoints() const {
    return mount_points_;
  }

 private:
  std::vector<std::filesystem::path> mount_points_;
};

}  // namespace puu::engine

#endif  // PUU_ENGINE_UTIL_H_
