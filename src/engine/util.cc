// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/util.h"

#include <fcntl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <charconv>
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
#include <fmt/base.h>
#include <fmt/format.h>
#include <openssl/crypto.h>
#include <openssl/err.h>
#include <openssl/evp.h>
#include <openssl/rand.h>

#include "base/secret_string.h"
#include "base/status.h"

namespace puu::engine {
namespace {

constexpr std::string_view kMountInfoPath = "/proc/self/mountinfo";

constexpr size_t kMountPointField = 4;

bool IsExecutable(const std::filesystem::path& path) {
  struct stat info = {};
  if (::stat(path.c_str(), &info) != 0) {
    return false;
  }
  return S_ISREG(info.st_mode) &&
         ((info.st_mode & (S_IXUSR | S_IXGRP | S_IXOTH)) != 0U);
}

std::string UnescapeMountInfo(std::string_view field) {
  std::string out;
  out.reserve(field.size());
  for (size_t i = 0; i < field.size(); ++i) {
    if (field[i] != '\\' || i + 3 >= field.size()) {
      out.push_back(field[i]);
      continue;
    }
    const std::string_view digits = field.substr(i + 1, 3);
    unsigned value = 0;
    const auto [end, error] =
        std::from_chars(digits.data(), digits.data() + digits.size(), value, 8);
    if (error != std::errc() || end != digits.data() + digits.size()) {
      out.push_back(field[i]);
      continue;
    }
    out.push_back(static_cast<char>(value));
    i += 3;
  }
  return out;
}

struct MountInfoEntry {
  std::filesystem::path mount_point;
  std::filesystem::path source;
};

std::vector<MountInfoEntry> ParseMountInfoEntries(std::string_view text) {
  std::vector<MountInfoEntry> entries;
  while (!text.empty()) {
    const size_t newline = text.find('\n');
    const std::string_view line = text.substr(0, newline);
    text = newline == std::string_view::npos ? std::string_view()
                                             : text.substr(newline + 1);

    std::string_view rest = line;
    std::string_view mount_point;
    for (size_t index = 0; index <= kMountPointField; ++index) {
      const size_t start = rest.find_first_not_of(" \t");
      if (start == std::string_view::npos) {
        mount_point = {};
        break;
      }
      rest.remove_prefix(start);
      const size_t end = rest.find_first_of(" \t");
      mount_point = rest.substr(0, end);
      rest =
          end == std::string_view::npos ? std::string_view() : rest.substr(end);
    }
    if (mount_point.empty()) {
      continue;
    }

    std::string_view source;
    const size_t separator = line.rfind(" - ");
    if (separator != std::string_view::npos) {
      rest = line.substr(separator + 3);
      const size_t fstype_end = rest.find(' ');
      if (fstype_end != std::string_view::npos) {
        rest.remove_prefix(fstype_end + 1);
        source = rest.substr(0, rest.find(' '));
      }
    }
    entries.push_back(
        {UnescapeMountInfo(mount_point), UnescapeMountInfo(source)});
  }
  return entries;
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

std::filesystem::path Canonical(const std::filesystem::path& path) {
  std::error_code error;
  std::filesystem::path const resolved =
      std::filesystem::canonical(path, error);
  return error ? path : resolved;
}

Status WriteBytes(const std::filesystem::path& path,
                  std::span<const uint8_t> data, mode_t mode, int extra_flags) {
  const UniqueFd fd(
      ::open(path.c_str(),
             O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | extra_flags, mode));
  if (fd.Get() < 0) {
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

Status ReplaceFileBytes(const std::filesystem::path& path,
                        std::span<const uint8_t> data) {
  const std::filesystem::path temporary =
      path.parent_path() / ("." + path.filename().string() + ".tmp");
  UniqueFd fd(::open(temporary.c_str(),
                     O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644));
  if (fd.Get() < 0) {
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
  fd = UniqueFd();
  if (::rename(temporary.c_str(), path.c_str()) != 0) {
    const int error = errno;
    std::error_code ignored;
    std::filesystem::remove(temporary, ignored);
    return ErrnoToStatus(error, fmt::format("rename {} to {}",
                                            temporary.string(), path.string()));
  }
  return absl::OkStatus();
}

namespace {

StatusOr<std::string> ReadMountInfo() {
  std::ifstream file{std::string(kMountInfoPath)};
  if (!file.is_open()) {
    return InternalError("failed to open {}", kMountInfoPath);
  }
  std::string text{std::istreambuf_iterator<char>(file),
                   std::istreambuf_iterator<char>()};
  if (file.bad()) {
    return InternalError("failed to read {}", kMountInfoPath);
  }
  return text;
}

}  // namespace

StatusOr<std::set<std::filesystem::path>> MountedSources() {
  PUU_ASSIGN_OR_RETURN(const std::string text, ReadMountInfo());
  std::set<std::filesystem::path> sources;
  for (const MountInfoEntry& entry : ParseMountInfoEntries(text)) {
    if (entry.source.native().starts_with("/dev/")) {
      sources.insert(Canonical(entry.source));
    }
  }
  return sources;
}

UniqueFd::UniqueFd(int fd) : fd_(fd) {}

UniqueFd::~UniqueFd() {
  if (fd_ >= 0) {
    ::close(fd_);
  }
}

UniqueFd::UniqueFd(UniqueFd&& other) noexcept
    : fd_(std::exchange(other.fd_, -1)) {}

UniqueFd& UniqueFd::operator=(UniqueFd&& other) noexcept {
  std::swap(fd_, other.fd_);
  return *this;
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

bool WhichExists(std::string_view name) { return FindInPath(name).has_value(); }

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
  if (fd.Get() < 0) {
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

std::vector<std::filesystem::path> ParseMountInfo(std::string_view text) {
  std::vector<std::filesystem::path> mount_points;
  for (MountInfoEntry& entry : ParseMountInfoEntries(text)) {
    mount_points.push_back(std::move(entry.mount_point));
  }
  return mount_points;
}

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
  PUU_ASSIGN_OR_RETURN(const std::string text, ReadMountInfo());
  Mounts mounts;
  mounts.mount_points_ = ParseMountInfo(text);
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
