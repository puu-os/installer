// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/k3s.h"

#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <string>
#include <string_view>

#include <openssl/crypto.h>

#include "engine/util.h"

namespace puu::engine {
namespace {

constexpr char kEnvironment[] = "puu-cluster.env";
constexpr char kServiceEnvironment[] = "k3s.service.env";
constexpr char kEnvironmentTarget[] = "/etc/rancher/k3s/puu-cluster.env";

bool ValidEncoding(std::string_view text, bool multiline = false) {
  for (size_t i = 0; i < text.size();) {
    const auto first = static_cast<unsigned char>(text[i++]);
    uint32_t code = first;
    size_t extra = 0;
    uint32_t minimum = 0;
    if (first >= 0xc2 && first <= 0xdf) {
      code &= 0x1f;
      extra = 1;
      minimum = 0x80;
    } else if (first >= 0xe0 && first <= 0xef) {
      code &= 0x0f;
      extra = 2;
      minimum = 0x800;
    } else if (first >= 0xf0 && first <= 0xf4) {
      code &= 0x07;
      extra = 3;
      minimum = 0x10000;
    } else if (first >= 0x80) {
      return false;
    }
    if (extra > text.size() - i) {
      return false;
    }
    for (size_t j = 0; j < extra; ++j) {
      const auto next = static_cast<unsigned char>(text[i++]);
      if ((next & 0xc0) != 0x80) {
        return false;
      }
      code = (code << 6) | (next & 0x3f);
    }
    if (code < minimum || code > 0x10ffff ||
        (code >= 0xd800 && code <= 0xdfff) || code == 0xfeff ||
        (code >= 0xfdd0 && code <= 0xfdef) || (code & 0xfffe) == 0xfffe ||
        code == 0 || (!multiline && (code == '\r' || code == '\n'))) {
      return false;
    }
  }
  return true;
}

Status SyncDirectory(int fd) {
  if (::fsync(fd) != 0) {
    return ErrnoToStatus(errno, "sync k3s directory");
  }
  return absl::OkStatus();
}

StatusOr<UniqueFd> OpenDirectory(int parent, const char* name,
                                 bool create = false) {
  if (create) {
    if (::mkdirat(parent, name, 0755) == 0) {
      PUU_RETURN_IF_ERROR(SyncDirectory(parent));
    } else if (errno != EEXIST) {
      return ErrnoToStatus(errno, "create k3s parent directory");
    }
  }
  UniqueFd fd(
      ::openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC));
  if (!fd) {
    return ErrnoToStatus(errno, "open k3s directory without symlinks");
  }
  return fd;
}

StatusOr<UniqueFd> OpenDeployment(const std::filesystem::path& path) {
  if (path.empty()) {
    return InvalidArgumentError("empty k3s deployment path");
  }
  PUU_ASSIGN_OR_RETURN(UniqueFd fd,
                       OpenDirectory(AT_FDCWD, path.is_absolute() ? "/" : "."));
  for (const auto& component : path.relative_path()) {
    if (component == ".." ||
        component.native().find('\0') != std::string::npos) {
      return InvalidArgumentError("invalid k3s deployment path");
    }
    if (component.empty() || component == ".") {
      continue;
    }
    PUU_ASSIGN_OR_RETURN(fd, OpenDirectory(fd.Get(), component.c_str()));
  }
  return fd;
}

StatusOr<std::string> ReadDefaults(int root) {
  PUU_ASSIGN_OR_RETURN(UniqueFd usr, OpenDirectory(root, "usr"));
  PUU_ASSIGN_OR_RETURN(UniqueFd lib, OpenDirectory(usr.Get(), "lib"));
  PUU_ASSIGN_OR_RETURN(UniqueFd systemd, OpenDirectory(lib.Get(), "systemd"));
  PUU_ASSIGN_OR_RETURN(UniqueFd system, OpenDirectory(systemd.Get(), "system"));
  UniqueFd fd(::openat(system.Get(), kServiceEnvironment,
                       O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC));
  if (!fd) {
    return ErrnoToStatus(errno, "open k3s image defaults");
  }
  struct stat st{};
  if (::fstat(fd.Get(), &st) != 0) {
    return ErrnoToStatus(errno, "stat k3s image defaults");
  }
  if (!S_ISREG(st.st_mode)) {
    return InvalidArgumentError("k3s image defaults must be a regular file");
  }
  std::string text;
  std::array<char, 4096> buffer{};
  for (;;) {
    const ssize_t count = ::read(fd.Get(), buffer.data(), buffer.size());
    if (count < 0) {
      if (errno == EINTR) {
        continue;
      }
      return ErrnoToStatus(errno, "read k3s image defaults");
    }
    if (count == 0) {
      break;
    }
    if (text.size() + static_cast<size_t>(count) > 1024 * 1024) {
      return InvalidArgumentError("k3s image defaults are too large");
    }
    text.append(buffer.data(), static_cast<size_t>(count));
  }
  if (!ValidEncoding(text, true)) {
    return InvalidArgumentError("invalid encoding in k3s image defaults");
  }
  return text;
}

bool HorizontalSpace(char c) { return c == ' ' || c == '\t' || c == '\r'; }

std::string ImagePath(std::string_view text) {
  enum class State {
    kLeading,
    kUnquoted,
    kSingle,
    kDouble,
    kEscape,
    kDoubleEscape
  };
  std::string path;
  size_t pos = 0;
  while (pos < text.size()) {
    while (pos < text.size() &&
           (HorizontalSpace(text[pos]) || text[pos] == '\n')) {
      ++pos;
    }
    const size_t start = pos;
    if (pos == text.size()) {
      break;
    }
    if (text[pos] == '#' || text[pos] == ';') {
      const size_t end = text.find('\n', pos);
      pos = end == text.npos ? text.size() : end + 1;
      continue;
    }
    while (pos < text.size() && text[pos] != '=' && text[pos] != '\n') {
      ++pos;
    }
    if (pos == text.size() || text[pos] != '=') {
      continue;
    }
    const bool is_path = Trim(text.substr(start, pos - start)) == "PATH";
    ++pos;
    State state = State::kLeading;
    std::string value;
    size_t trailing = std::string::npos;
    while (pos < text.size()) {
      const char c = text[pos++];
      if ((state == State::kLeading || state == State::kUnquoted) &&
          c == '\n') {
        break;
      }
      switch (state) {
        case State::kLeading:
          if (c == '\'') {
            state = State::kSingle;
          } else if (c == '"') {
            state = State::kDouble;
          } else if (c == '\\') {
            state = State::kEscape;
          } else if (!HorizontalSpace(c)) {
            state = State::kUnquoted;
            value.push_back(c);
          }
          break;
        case State::kUnquoted:
          if (c == '\\') {
            state = State::kEscape;
            trailing = std::string::npos;
          } else {
            if (!HorizontalSpace(c)) {
              trailing = std::string::npos;
            } else if (trailing == std::string::npos) {
              trailing = value.size();
            }
            value.push_back(c);
          }
          break;
        case State::kSingle:
        case State::kDouble:
          if (c == (state == State::kSingle ? '\'' : '"')) {
            state = State::kLeading;
          } else if (state == State::kDouble && c == '\\') {
            state = State::kDoubleEscape;
          } else {
            value.push_back(c);
          }
          break;
        case State::kEscape:
          state = State::kUnquoted;
          if (c != '\n') {
            value.push_back(c);
          }
          break;
        case State::kDoubleEscape:
          state = State::kDouble;
          if (c != '\n') {
            if (c != '\\' && c != '"' && c != '$' && c != '`') {
              value.push_back('\\');
            }
            value.push_back(c);
          }
          break;
      }
    }
    if (state == State::kUnquoted && trailing != std::string::npos) {
      value.resize(trailing);
    }
    if (is_path) {
      path = std::move(value);
    }
  }
  return path;
}

Status WriteAll(int fd, std::string_view text) {
  while (!text.empty()) {
    const ssize_t count = ::write(fd, text.data(), text.size());
    if (count < 0) {
      if (errno == EINTR) {
        continue;
      }
      return ErrnoToStatus(errno, "write k3s environment");
    }
    if (count == 0) {
      return InternalError("zero-length write of k3s environment");
    }
    text.remove_prefix(static_cast<size_t>(count));
  }
  return absl::OkStatus();
}

Status WriteAssignment(int fd, std::string_view key, std::string_view value) {
  struct Buffer {
    std::array<char, 512> data{};
    ~Buffer() { OPENSSL_cleanse(data.data(), data.size()); }
  } buffer;
  PUU_RETURN_IF_ERROR(WriteAll(fd, key));
  PUU_RETURN_IF_ERROR(WriteAll(fd, "=\""));
  size_t used = 0;
  for (const char c : value) {
    if (used + 2 > buffer.data.size()) {
      PUU_RETURN_IF_ERROR(WriteAll(fd, {buffer.data.data(), used}));
      used = 0;
    }
    if (c == '\\' || c == '"' || c == '$' || c == '`') {
      buffer.data[used++] = '\\';
    }
    buffer.data[used++] = c;
  }
  PUU_RETURN_IF_ERROR(WriteAll(fd, {buffer.data.data(), used}));
  return WriteAll(fd, "\"\n");
}

StatusOr<std::string> TemporaryName() {
  std::array<uint8_t, 16> random{};
  PUU_RETURN_IF_ERROR(RandomBytes(random));
  constexpr char hex[] = "0123456789abcdef";
  std::string name = ".puu-k3s-";
  for (const uint8_t byte : random) {
    name.push_back(hex[byte >> 4]);
    name.push_back(hex[byte & 15]);
  }
  return name;
}

struct TemporaryEntry {
  int parent;
  std::string name;
  bool retained = false;
  ~TemporaryEntry() {
    if (!retained && !name.empty()) {
      ::unlinkat(parent, name.c_str(), 0);
    }
  }
  Status Backup(const char* destination) {
    for (int attempt = 0; attempt < 16; ++attempt) {
      PUU_ASSIGN_OR_RETURN(auto candidate, TemporaryName());
      if (::linkat(parent, destination, parent, candidate.c_str(), 0) == 0) {
        name = std::move(candidate);
        return absl::OkStatus();
      }
      if (errno == ENOENT) {
        return absl::OkStatus();
      }
      if (errno != EEXIST) {
        return ErrnoToStatus(errno, "retain previous k3s configuration");
      }
    }
    return InternalError("cannot allocate unique k3s backup name");
  }
  Status Restore(const char* destination) {
    if (name.empty()) {
      if (::unlinkat(parent, destination, 0) != 0) {
        return ErrnoToStatus(errno, "remove failed k3s configuration");
      }
    } else {
      if (::renameat(parent, name.c_str(), parent, destination) != 0) {
        const int error = errno;
        retained = true;
        return ErrnoToStatus(
            error, "restore previous k3s configuration; retained " + name);
      }
      name.clear();
    }
    return SyncDirectory(parent);
  }
  Status Cleanup() {
    if (!retained && !name.empty()) {
      if (::unlinkat(parent, name.c_str(), 0) != 0) {
        return ErrnoToStatus(errno, "remove temporary k3s entry");
      }
      name.clear();
      return SyncDirectory(parent);
    }
    return absl::OkStatus();
  }
};

Status CheckDestination(int parent, const char* name) {
  struct stat st{};
  if (::fstatat(parent, name, &st, AT_SYMLINK_NOFOLLOW) != 0) {
    if (errno == ENOENT) {
      return absl::OkStatus();
    }
    return ErrnoToStatus(errno, "stat k3s destination");
  }
  if (!S_ISREG(st.st_mode) && !S_ISLNK(st.st_mode)) {
    return InvalidArgumentError("k3s destination must be a file or symlink");
  }
  return absl::OkStatus();
}

}  // namespace

Status StageK3s(const std::filesystem::path& deployment,
                const K3sConfig& config, SecretString token) {
  if (config.role == K3sRole::kServer && !config.node_name.has_value()) {
    return absl::OkStatus();
  }
  if (config.role != K3sRole::kServer && config.role != K3sRole::kAgent) {
    return InvalidArgumentError("invalid k3s role");
  }
  std::string_view normalized_token = Trim(token.Value());
  if (config.role == K3sRole::kAgent &&
      (!config.server_url.has_value() || normalized_token.empty())) {
    return InvalidArgumentError("k3s agent requires server URL and token");
  }
  if (!ValidEncoding(token.Value())) {
    return InvalidArgumentError("invalid k3s token encoding or line break");
  }
  if (config.node_name.has_value()) {
    PUU_RETURN_IF_ERROR(Hostname::Validate(config.node_name->Value()));
  }
  if (config.server_url.has_value()) {
    PUU_RETURN_IF_ERROR(ServerUrl::Validate(config.server_url->Value()));
    if (!ValidEncoding(config.server_url->Value())) {
      return InvalidArgumentError("invalid k3s server URL encoding");
    }
  }
  PUU_ASSIGN_OR_RETURN(UniqueFd root, OpenDeployment(deployment));
  PUU_ASSIGN_OR_RETURN(const std::string defaults, ReadDefaults(root.Get()));
  std::string path;
  if (config.role == K3sRole::kAgent) {
    path = ImagePath(defaults);
    if (Trim(path).empty()) {
      return InvalidArgumentError("k3s image defaults require PATH");
    }
  }
  PUU_ASSIGN_OR_RETURN(UniqueFd etc, OpenDirectory(root.Get(), "etc", true));
  PUU_ASSIGN_OR_RETURN(UniqueFd rancher,
                       OpenDirectory(etc.Get(), "rancher", true));
  if (::mkdirat(rancher.Get(), "k3s", 0700) == 0) {
    PUU_RETURN_IF_ERROR(SyncDirectory(rancher.Get()));
  } else if (errno != EEXIST) {
    return ErrnoToStatus(errno, "create private k3s directory");
  }
  PUU_ASSIGN_OR_RETURN(UniqueFd directory, OpenDirectory(rancher.Get(), "k3s"));
  if (::fchmod(directory.Get(), 0700) != 0) {
    return ErrnoToStatus(errno, "protect k3s directory");
  }
  PUU_RETURN_IF_ERROR(SyncDirectory(directory.Get()));
  PUU_ASSIGN_OR_RETURN(UniqueFd systemd,
                       OpenDirectory(etc.Get(), "systemd", true));
  PUU_ASSIGN_OR_RETURN(UniqueFd system,
                       OpenDirectory(systemd.Get(), "system", true));
  PUU_RETURN_IF_ERROR(CheckDestination(directory.Get(), kEnvironment));
  PUU_RETURN_IF_ERROR(CheckDestination(system.Get(), kServiceEnvironment));

  TemporaryEntry file{directory.Get(), {}};
  TemporaryEntry link{system.Get(), {}};
  TemporaryEntry previous_file{directory.Get(), {}};
  TemporaryEntry previous_link{system.Get(), {}};
  bool file_published = false;
  bool link_published = false;
  const Status result = [&]() -> Status {
    UniqueFd output;
    for (int attempt = 0; attempt < 16; ++attempt) {
      PUU_ASSIGN_OR_RETURN(auto name, TemporaryName());
      output.Reset(
          ::openat(directory.Get(), name.c_str(),
                   O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600));
      if (output) {
        file.name = std::move(name);
        break;
      }
      if (errno != EEXIST) {
        return ErrnoToStatus(errno, "create temporary k3s environment");
      }
    }
    if (!output) {
      return InternalError("cannot allocate unique k3s environment name");
    }
    if (::fchmod(output.Get(), 0600) != 0) {
      return ErrnoToStatus(errno, "protect k3s environment");
    }
    if (config.role == K3sRole::kServer) {
      PUU_RETURN_IF_ERROR(WriteAll(output.Get(), defaults));
      if (!defaults.empty() && defaults.back() != '\n') {
        PUU_RETURN_IF_ERROR(WriteAll(output.Get(), "\n"));
      }
    } else {
      PUU_RETURN_IF_ERROR(WriteAssignment(output.Get(), "PATH", path));
      PUU_RETURN_IF_ERROR(WriteAssignment(output.Get(), "K3S_EXEC", "agent"));
      PUU_RETURN_IF_ERROR(
          WriteAssignment(output.Get(), "K3S_URL", config.server_url->Value()));
      PUU_RETURN_IF_ERROR(
          WriteAssignment(output.Get(), "K3S_TOKEN", normalized_token));
    }
    if (config.node_name.has_value()) {
      PUU_RETURN_IF_ERROR(WriteAssignment(output.Get(), "K3S_NODE_NAME",
                                          config.node_name->Value()));
    }
    if (::fsync(output.Get()) != 0) {
      return ErrnoToStatus(errno, "sync k3s environment");
    }
    const int output_fd = output.Release();
    if (::close(output_fd) != 0) {
      return ErrnoToStatus(errno, "close k3s environment");
    }
    for (int attempt = 0; attempt < 16; ++attempt) {
      PUU_ASSIGN_OR_RETURN(auto name, TemporaryName());
      if (::symlinkat(kEnvironmentTarget, system.Get(), name.c_str()) == 0) {
        link.name = std::move(name);
        break;
      }
      if (errno != EEXIST) {
        return ErrnoToStatus(errno, "create temporary k3s environment link");
      }
    }
    if (link.name.empty()) {
      return InternalError("cannot allocate unique k3s link name");
    }
    PUU_RETURN_IF_ERROR(previous_file.Backup(kEnvironment));
    PUU_RETURN_IF_ERROR(previous_link.Backup(kServiceEnvironment));
    if (::renameat(directory.Get(), file.name.c_str(), directory.Get(),
                   kEnvironment) != 0) {
      return ErrnoToStatus(errno, "publish k3s environment");
    }
    file.name.clear();
    file_published = true;
    PUU_RETURN_IF_ERROR(SyncDirectory(directory.Get()));
    if (::renameat(system.Get(), link.name.c_str(), system.Get(),
                   kServiceEnvironment) != 0) {
      return ErrnoToStatus(errno, "publish k3s environment link");
    }
    link.name.clear();
    link_published = true;
    return SyncDirectory(system.Get());
  }();
  Status status = result;
  const auto record = [&status](const Status& error) {
    if (!error.ok()) {
      if (status.ok()) {
        status = error;
      } else {
        status = Status(status.code(), std::string(status.message()) + "; " +
                                           std::string(error.message()));
      }
    }
  };
  if (!status.ok()) {
    if (link_published) {
      record(previous_link.Restore(kServiceEnvironment));
    }
    if (file_published) {
      record(previous_file.Restore(kEnvironment));
    }
  }
  record(file.Cleanup());
  record(link.Cleanup());
  record(previous_file.Cleanup());
  record(previous_link.Cleanup());
  return status;
}

}  // namespace puu::engine
