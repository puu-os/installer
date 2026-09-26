// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/util.h"

#include <fcntl.h>
#include <libmount/libmount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <functional>
#include <ios>
#include <iterator>
#include <limits>
#include <optional>
#include <set>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <thread>
#include <utility>
#include <vector>

#include <absl/status/status.h>
#include <fmt/core.h>
#include <fmt/format.h>
#include <openssl/crypto.h>
#include <openssl/err.h>
#include <openssl/evp.h>
#include <openssl/rand.h>

#include "base/secret_string.h"
#include "base/status.h"

namespace puu::engine {
namespace {

struct MountTableDeleter {
  void operator()(libmnt_table* tb) const {
    if (tb != nullptr) {
      mnt_free_table(tb);
    }
  }
};
using UniqueMountTable = std::unique_ptr<libmnt_table, MountTableDeleter>;

struct MountIterDeleter {
  void operator()(libmnt_iter* itr) const {
    if (itr != nullptr) {
      mnt_free_iter(itr);
    }
  }
};
using UniqueMountIter = std::unique_ptr<libmnt_iter, MountIterDeleter>;

StatusOr<UniqueMountTable> ReadProcMountTable() {
  UniqueMountTable table(mnt_new_table());
  if (!table) {
    return InternalError("failed to allocate mount table");
  }
  if (mnt_table_parse_file(table.get(), "/proc/self/mountinfo") != 0) {
    return InternalError("failed to parse /proc/self/mountinfo");
  }
  return table;
}

bool IsExecutable(const std::filesystem::path& path) {
  struct stat info = {};
  if (::stat(path.c_str(), &info) != 0) {
    return false;
  }
  return S_ISREG(info.st_mode) &&
         ((info.st_mode & (S_IXUSR | S_IXGRP | S_IXOTH)) != 0U);
}

}  // namespace

Status FilesystemError(const std::error_code& error,
                       std::string_view operation) {
  return ErrnoToStatus(error.value(), operation);
}

Status CreateDirectories(const std::filesystem::path& path) {
  std::error_code error;
  std::filesystem::create_directories(path, error);
  if (error) {
    return FilesystemError(error, fmt::format("mkdir {}", path.string()));
  }
  return absl::OkStatus();
}

bool IsDirectory(const std::filesystem::path& path) {
  std::error_code error;
  return std::filesystem::is_directory(path, error);
}

bool IsRegularFile(const std::filesystem::path& path) {
  std::error_code error;
  return std::filesystem::is_regular_file(path, error);
}

bool Exists(const std::filesystem::path& path) {
  std::error_code error;
  return std::filesystem::exists(std::filesystem::symlink_status(path, error));
}

std::filesystem::path Canonical(const std::filesystem::path& path) {
  std::error_code error;
  std::filesystem::path const resolved =
      std::filesystem::canonical(path, error);
  return error ? path : resolved;
}
Status CopyFile(const std::filesystem::path& source,
                const std::filesystem::path& destination) {
  std::error_code error;
  std::filesystem::copy_file(source, destination,
                             std::filesystem::copy_options::overwrite_existing,
                             error);
  if (error) {
    return FilesystemError(error, fmt::format("copy {} to {}", source.string(),
                                              destination.string()));
  }
  return absl::OkStatus();
}
Status Rename(const std::filesystem::path& source,
              const std::filesystem::path& destination) {
  std::error_code error;
  std::filesystem::rename(source, destination, error);
  if (error) {
    return FilesystemError(
        error,
        fmt::format("rename {} to {}", source.string(), destination.string()));
  }
  return absl::OkStatus();
}

StatusOr<std::filesystem::path> ReadSymlink(const std::filesystem::path& path) {
  std::error_code error;
  const std::filesystem::path target =
      std::filesystem::read_symlink(path, error);
  if (error) {
    return FilesystemError(error, fmt::format("readlink {}", path.string()));
  }
  return target;
}

Status CreateSymlink(const std::filesystem::path& target,
                     const std::filesystem::path& link_path) {
  std::error_code error;
  std::filesystem::create_symlink(target, link_path, error);
  if (error) {
    return FilesystemError(
        error,
        fmt::format("symlink {} to {}", link_path.string(), target.string()));
  }
  return absl::OkStatus();
}

Status RemoveFile(const std::filesystem::path& path) {
  std::error_code error;
  std::filesystem::remove(path, error);
  if (error) {
    return FilesystemError(error, fmt::format("remove {}", path.string()));
  }
  return absl::OkStatus();
}

Status RemoveTree(const std::filesystem::path& path) {
  std::error_code error;
  std::filesystem::remove_all(path, error);
  if (error) {
    return FilesystemError(error, fmt::format("remove {}", path.string()));
  }
  return absl::OkStatus();
}

Status WriteBytes(const std::filesystem::path& path,
                  std::span<const uint8_t> data, mode_t mode, int extra_flags) {
  const UniqueFd fd(
      ::open(path.c_str(),
             O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | extra_flags, mode));
  if (!fd) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("open {}", path.string()));
  }

  if (::fchmod(fd.Get(), mode) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("chmod {}", path.string()));
  }
  size_t done = 0;
  while (done < data.size()) {
    const ssize_t written =
        ::write(fd.Get(), data.data() + done, data.size() - done);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      const int error = errno;
      return ErrnoToStatus(error, fmt::format("write {}", path.string()));
    }
    done += static_cast<size_t>(written);
  }
  if (::fsync(fd.Get()) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("fsync {}", path.string()));
  }
  return absl::OkStatus();
}

Status WriteFile(const std::filesystem::path& path, std::string_view content,
                 mode_t mode, int extra_flags) {
  return WriteBytes(
      path,
      std::span<const uint8_t>(reinterpret_cast<const uint8_t*>(content.data()),
                               content.size()),
      mode, extra_flags);
}

Status ReplaceFileBytes(const std::filesystem::path& path,
                        std::span<const uint8_t> data) {
  const std::filesystem::path temporary =
      path.parent_path() / ("." + path.filename().string() + ".tmp");
  UniqueFd fd(::open(temporary.c_str(),
                     O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644));
  if (!fd) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("open {}", temporary.string()));
  }
  size_t done = 0;
  while (done < data.size()) {
    const ssize_t written =
        ::write(fd.Get(), data.data() + done, data.size() - done);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      const int error = errno;
      return ErrnoToStatus(error, fmt::format("write {}", temporary.string()));
    }
    done += static_cast<size_t>(written);
  }
  if (::fsync(fd.Get()) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("fsync {}", temporary.string()));
  }
  fd.Reset();
  if (::rename(temporary.c_str(), path.c_str()) != 0) {
    const int error = errno;
    std::error_code ignored;
    std::filesystem::remove(temporary, ignored);
    return ErrnoToStatus(error, fmt::format("rename {} to {}",
                                            temporary.string(), path.string()));
  }
  return absl::OkStatus();
}

Status ReplaceFile(const std::filesystem::path& path,
                   std::string_view content) {
  return ReplaceFileBytes(
      path,
      std::span<const uint8_t>(reinterpret_cast<const uint8_t*>(content.data()),
                               content.size()));
}

StatusOr<std::set<std::filesystem::path>> MountedSources() {
  PUU_ASSIGN_OR_RETURN(const UniqueMountTable table, ReadProcMountTable());
  UniqueMountIter itr(mnt_new_iter(MNT_ITER_FORWARD));
  if (!itr) {
    return InternalError("failed to allocate mount iterator");
  }
  std::set<std::filesystem::path> sources;
  libmnt_fs* fs = nullptr;
  while (mnt_table_next_fs(table.get(), itr.get(), &fs) == 0) {
    const char* src = mnt_fs_get_srcpath(fs);
    if (src == nullptr) {
      src = mnt_fs_get_source(fs);
    }
    if (src != nullptr && std::string_view(src).starts_with("/dev/")) {
      sources.insert(Canonical(src));
    }
  }
  return sources;
}

UniqueFd::UniqueFd(int fd) : fd_(fd) {}

UniqueFd::~UniqueFd() { Reset(); }

UniqueFd::UniqueFd(UniqueFd&& other) noexcept : fd_(other.Release()) {}

UniqueFd& UniqueFd::operator=(UniqueFd&& other) noexcept {
  if (this != &other) {
    Reset(other.Release());
  }
  return *this;
}

void UniqueFd::Reset(int fd) noexcept {
  if (fd_ >= 0) {
    ::close(fd_);
  }
  fd_ = fd;
}

int UniqueFd::Release() noexcept { return std::exchange(fd_, -1); }

StatusOr<Pipe> CreatePipe(int flags) {
  std::array<int, 2> fds = {-1, -1};
  if (::pipe2(fds.data(), flags) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, "pipe2");
  }
  return Pipe{.read = UniqueFd(fds[0]), .write = UniqueFd(fds[1])};
}

Status RandomBytes(std::span<uint8_t> buffer) {
  if (buffer.empty()) {
    return absl::OkStatus();
  }
  if (buffer.size() > static_cast<size_t>(std::numeric_limits<int>::max())) {
    return InvalidArgumentError("random request of {} bytes is too large",
                                buffer.size());
  }
  if (RAND_bytes(buffer.data(), static_cast<int>(buffer.size())) != 1) {
    std::array<char, 256> reason = {};
    ERR_error_string_n(ERR_get_error(), reason.data(), reason.size());
    return InternalError("RAND_bytes failed: {}", reason.data());
  }
  return absl::OkStatus();
}

void SyncAll() { ::sync(); }

bool RunningAsRoot() { return ::geteuid() == 0; }

bool RunningInContainer() {
  const char* runtime = std::getenv("container");
  return runtime != nullptr && std::string_view(runtime) == "podman";
}

Status WaitUntil(const std::function<bool()>& predicate,
                 std::chrono::milliseconds timeout,
                 std::chrono::milliseconds interval) {
  const auto deadline = std::chrono::steady_clock::now() + timeout;
  while (true) {
    if (predicate()) {
      return absl::OkStatus();
    }
    if (std::chrono::steady_clock::now() >= deadline) {
      return absl::DeadlineExceededError(
          fmt::format("timed out after {}ms", timeout.count()));
    }
    std::this_thread::sleep_for(interval);
  }
}

std::string HumanSize(uint64_t bytes) {
  constexpr std::array<std::string_view, 6> kUnits = {"B",   "KiB", "MiB",
                                                      "GiB", "TiB", "PiB"};
  auto size = static_cast<double>(bytes);
  size_t unit = 0;
  while (size >= 1024.0 && unit + 1 < kUnits.size()) {
    size /= 1024.0;
    ++unit;
  }
  return fmt::format("{:.1f} {}", size, kUnits[unit]);
}

std::string CleanLogLine(std::string_view line) {
  const size_t carriage = line.rfind('\r');
  const std::string_view visible =
      carriage == std::string_view::npos ? line : line.substr(carriage + 1);

  std::string out;
  out.reserve(visible.size());
  for (size_t i = 0; i < visible.size(); ++i) {
    const auto character = static_cast<unsigned char>(visible[i]);
    if (character == 0x1b) {
      if (i + 1 < visible.size() && visible[i + 1] == '[') {
        i += 2;
        while (i < visible.size() && (visible[i] < '@' || visible[i] > '~')) {
          ++i;
        }
      }
      continue;
    }
    if (character == '\t') {
      out.push_back(' ');
      continue;
    }

    if (character < 0x20 || character == 0x7f) {
      continue;
    }
    out.push_back(visible[i]);
  }

  const size_t last = out.find_last_not_of(" \t");
  out.resize(last == std::string::npos ? 0 : last + 1);
  return out;
}

std::filesystem::path DefaultShell() {
  const char* shell = std::getenv("SHELL");
  if (shell == nullptr || *shell == '\0') {
    return {"/bin/sh"};
  }
  return {shell};
}

std::optional<std::filesystem::path> FindInPath(std::string_view name) {
  if (name.empty()) {
    return std::nullopt;
  }
  if (name.find('/') != std::string_view::npos) {
    const std::filesystem::path candidate(name);
    return IsExecutable(candidate) ? std::optional(candidate) : std::nullopt;
  }

  const char* search_path = std::getenv("PATH");
  if (search_path == nullptr || search_path[0] == '\0') {
    search_path = "/usr/bin:/bin";
  }

  std::string_view remaining(search_path);
  while (!remaining.empty()) {
    const size_t separator = remaining.find(':');
    const std::string_view directory = remaining.substr(0, separator);

    std::filesystem::path candidate =
        std::filesystem::path(directory.empty() ? "." : directory) / name;
    if (IsExecutable(candidate)) {
      return candidate;
    }
    if (separator == std::string_view::npos) {
      break;
    }
    remaining.remove_prefix(separator + 1);
  }
  return std::nullopt;
}

StatusOr<std::string> ReadFirstLine(const std::filesystem::path& path) {
  std::ifstream file(path);
  if (!file.is_open()) {
    return InternalError("failed to open {}", path.string());
  }
  std::string line;
  std::getline(file, line);
  if (file.bad()) {
    return InternalError("failed to read {}", path.string());
  }
  return line;
}

StatusOr<SecretString> ReadFirstSecretLine(const std::filesystem::path& path) {
  const UniqueFd fd(::open(path.c_str(), O_RDONLY | O_CLOEXEC | O_NOFOLLOW));
  if (!fd) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("open {}", path.string()));
  }
  struct stat info = {};
  if (::fstat(fd.Get(), &info) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("stat {}", path.string()));
  }
  if (!S_ISREG(info.st_mode)) {
    return InvalidArgumentError("{} is not a regular file", path.string());
  }
  if (info.st_uid != ::geteuid()) {
    return InvalidArgumentError("{} is not owned by the current user",
                                path.string());
  }
  if ((info.st_mode & 077) != 0) {
    return InvalidArgumentError(
        "{} must be readable by the owner only (chmod 600)", path.string());
  }
  std::array<char, 4096> buffer{};
  ssize_t count = 0;
  do {
    count = ::read(fd.Get(), buffer.data(), buffer.size());
  } while (count < 0 && errno == EINTR);
  if (count < 0) {
    const int error = errno;
    OPENSSL_cleanse(buffer.data(), buffer.size());
    return ErrnoToStatus(error, fmt::format("read {}", path.string()));
  }
  const char* newline = static_cast<const char*>(
      std::memchr(buffer.data(), '\n', static_cast<size_t>(count)));
  if (newline == nullptr && static_cast<size_t>(count) == buffer.size()) {
    OPENSSL_cleanse(buffer.data(), buffer.size());
    return InvalidArgumentError("first line in {} exceeds 4095 bytes",
                                path.string());
  }
  size_t length = (newline != nullptr)
                      ? static_cast<size_t>(newline - buffer.data())
                      : static_cast<size_t>(count);
  if (length > 0 && buffer[length - 1] == '\r') {
    --length;
  }
  SecretString result(std::string_view(buffer.data(), length));
  OPENSSL_cleanse(buffer.data(), buffer.size());
  return result;
}

StatusOr<std::vector<uint8_t>> ReadFileBytes(
    const std::filesystem::path& path) {
  std::ifstream file(path, std::ios::binary);
  if (!file) {
    return InternalError("failed to open {}", path.string());
  }
  std::vector<uint8_t> bytes{std::istreambuf_iterator<char>(file),
                             std::istreambuf_iterator<char>()};
  if (file.bad()) {
    return InternalError("failed to read {}", path.string());
  }
  return bytes;
}

StatusOr<std::string> ReadFile(const std::filesystem::path& path) {
  std::ifstream file(path, std::ios::binary);
  if (!file) {
    return InternalError("failed to open {}", path.string());
  }
  std::string content{std::istreambuf_iterator<char>(file),
                      std::istreambuf_iterator<char>()};
  if (file.bad()) {
    return InternalError("failed to read {}", path.string());
  }
  return content;
}

std::string_view Trim(std::string_view text) {
  const size_t begin = text.find_first_not_of(" \t\r\n");
  if (begin == std::string_view::npos) {
    return {};
  }
  return text.substr(begin, text.find_last_not_of(" \t\r\n") - begin + 1);
}

StatusOr<std::array<uint8_t, 32>> Sha256(std::span<const uint8_t> data) {
  std::array<uint8_t, 32> digest{};
  unsigned length = 0;
  if (EVP_Digest(data.data(), data.size(), digest.data(), &length, EVP_sha256(),
                 nullptr) != 1) {
    return InternalError("SHA-256 digest failed");
  }
  return digest;
}

StatusOr<std::string> Sha256Hex(std::span<const uint8_t> data) {
  PUU_ASSIGN_OR_RETURN(const auto digest, Sha256(data));
  std::string hex;
  hex.reserve(digest.size() * 2);
  for (const uint8_t byte : digest) {
    fmt::format_to(std::back_inserter(hex), "{:02x}", byte);
  }
  return hex;
}

namespace {

std::vector<std::filesystem::path> MountPointsBelow(
    const std::vector<std::filesystem::path>& mount_points,
    const std::filesystem::path& root) {
  std::vector<std::filesystem::path> selected;
  for (const std::filesystem::path& point : mount_points) {
    if (std::ranges::mismatch(root, point).in1 == root.end()) {
      selected.push_back(point);
    }
  }

  std::ranges::sort(selected, [](const std::filesystem::path& left,
                                 const std::filesystem::path& right) {
    return left.native().size() > right.native().size();
  });
  return selected;
}

}  // namespace

StatusOr<Mounts> Mounts::Read() {
  PUU_ASSIGN_OR_RETURN(const UniqueMountTable table, ReadProcMountTable());
  UniqueMountIter itr(mnt_new_iter(MNT_ITER_FORWARD));
  if (!itr) {
    return InternalError("failed to allocate mount iterator");
  }
  Mounts mounts;
  libmnt_fs* fs = nullptr;
  while (mnt_table_next_fs(table.get(), itr.get(), &fs) == 0) {
    const char* target = mnt_fs_get_target(fs);
    if (target != nullptr && *target != '\0') {
      mounts.mount_points_.emplace_back(target);
    }
  }
  return mounts;
}

bool Mounts::IsMountPoint(const std::filesystem::path& path) const {
  const std::filesystem::path wanted = Canonical(path);
  return std::ranges::any_of(mount_points_,
                             [&wanted](const std::filesystem::path& point) {
                               return Canonical(point) == wanted;
                             });
}

std::vector<std::filesystem::path> Mounts::Below(
    const std::filesystem::path& root) const {
  return MountPointsBelow(mount_points_, root);
}

}  // namespace puu::engine
