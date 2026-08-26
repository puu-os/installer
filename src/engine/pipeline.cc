// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/pipeline.h"

#include <sched.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <fstream>
#include <map>
#include <memory>
#include <string>
#include <system_error>
#include <utility>
#include <vector>

#include <spdlog/spdlog.h>

#include "config.h"
#include "engine/util.h"

namespace puu::engine {
namespace {

constexpr std::array<std::pair<std::string_view, std::string_view>, 4>
    kChrootBindMounts = {
        {{"/proc", "proc"}, {"/sys", "sys"}, {"/dev", "dev"}, {"/run", "run"}}};

constexpr std::array<std::string_view, 5> kFixedKernelArgs = {
    "rw", "vt.global_cursor_default=0", "console=tty0", "threadirqs",
    "preempt=full"};

std::string_view FstabMountPoint(std::string_view line) {
  size_t position = line.find_first_not_of(" \t");
  if (position == std::string_view::npos || line[position] == '#') return {};
  std::string_view field;
  for (int index = 0; index < 2; ++index) {
    position = line.find_first_not_of(" \t", position);
    if (position == std::string_view::npos) return {};
    const size_t end = line.find_first_of(" \t", position);
    field = line.substr(position, end == std::string_view::npos
                                      ? std::string_view::npos
                                      : end - position);
    position = end;
  }
  return field;
}

std::filesystem::perms PermissionBits(mode_t mode) {
  return std::filesystem::perms{mode} & std::filesystem::perms::mask;
}

Status RemoveTree(const std::filesystem::path& path) {
  std::error_code error;
  std::filesystem::remove_all(path, error);
  if (error) {
    return FilesystemError(error, fmt::format("remove {}", path.string()));
  }
  return absl::OkStatus();
}

StatusOr<std::vector<std::filesystem::path>> TeardownMountPoints() {
  PUU_ASSIGN_OR_RETURN(const Mounts mounts, Mounts::Read());
  return mounts.Below(kTargetMount);
}

Status CopyXattrs(const std::filesystem::path& source,
                  const std::filesystem::path& destination) {
  ssize_t size = ::listxattr(source.c_str(), nullptr, 0);
  if (size < 0) {
    const int error = errno;

    if (error == ENOTSUP) return absl::OkStatus();
    return ErrnoToStatus(error, fmt::format("listxattr {}", source.string()));
  }
  std::string names(static_cast<size_t>(size), '\0');
  size = ::listxattr(source.c_str(), names.data(), names.size());
  if (size < 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("listxattr {}", source.string()));
  }
  names.resize(static_cast<size_t>(size));

  for (size_t start = 0; start < names.size();) {
    const size_t end = names.find('\0', start);
    const std::string name =
        names.substr(start, end == std::string::npos ? end : end - start);
    start = end == std::string::npos ? names.size() : end + 1;

    if (!name.starts_with("user.")) continue;

    ssize_t value_size = ::getxattr(source.c_str(), name.c_str(), nullptr, 0);
    if (value_size < 0) {
      const int error = errno;
      if (error == ENODATA) continue;
      return ErrnoToStatus(
          error, fmt::format("getxattr {} {}", source.string(), name));
    }
    std::string value(static_cast<size_t>(value_size), '\0');
    value_size =
        ::getxattr(source.c_str(), name.c_str(), value.data(), value.size());
    if (value_size < 0) {
      const int error = errno;
      if (error == ENODATA) continue;
      return ErrnoToStatus(
          error, fmt::format("getxattr {} {}", source.string(), name));
    }
    if (::setxattr(destination.c_str(), name.c_str(), value.data(),
                   static_cast<size_t>(value_size), 0) != 0) {
      const int error = errno;
      return ErrnoToStatus(
          error, fmt::format("setxattr {} {}", destination.string(), name));
    }
  }
  return absl::OkStatus();
}

Status SetPermissions(const std::filesystem::path& path,
                      std::filesystem::perms permissions) {
  std::error_code error;
  std::filesystem::permissions(path, permissions, error);
  if (error) {
    return FilesystemError(error, fmt::format("chmod {}", path.string()));
  }
  return absl::OkStatus();
}

Status CopyFileContents(const std::filesystem::path& source,
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

using HardLinkMap = std::map<std::pair<dev_t, ino_t>, std::filesystem::path>;

Status CopyTreeEntry(const std::filesystem::path& source,
                     const std::filesystem::path& destination,
                     HardLinkMap* hard_links);

Status CopyDirectory(const std::filesystem::path& source,
                     const std::filesystem::path& destination,
                     const struct stat& info, HardLinkMap* hard_links) {
  PUU_RETURN_IF_ERROR(CreateDirectories(destination));
  std::error_code error;
  std::filesystem::directory_iterator entries(source, error);
  if (error) {
    return FilesystemError(error, fmt::format("readdir {}", source.string()));
  }
  const std::filesystem::directory_iterator end;
  for (; entries != end; entries.increment(error)) {
    if (error) {
      return FilesystemError(error, fmt::format("readdir {}", source.string()));
    }
    PUU_RETURN_IF_ERROR(CopyTreeEntry(
        entries->path(), destination / entries->path().filename(), hard_links));
  }

  PUU_RETURN_IF_ERROR(
      SetPermissions(destination, PermissionBits(info.st_mode)));
  return CopyXattrs(source, destination);
}

Status CopySymlink(const std::filesystem::path& source,
                   const std::filesystem::path& destination) {
  std::error_code error;
  const std::filesystem::path target =
      std::filesystem::read_symlink(source, error);
  if (error) {
    return FilesystemError(error, fmt::format("readlink {}", source.string()));
  }
  std::filesystem::create_symlink(target, destination, error);
  if (error) {
    return FilesystemError(error,
                           fmt::format("symlink {}", destination.string()));
  }
  return absl::OkStatus();
}

Status CopyRegularFile(const std::filesystem::path& source,
                       const std::filesystem::path& destination,
                       const struct stat& info, HardLinkMap* hard_links) {
  const std::pair<dev_t, ino_t> key{info.st_dev, info.st_ino};
  if (info.st_nlink > 1) {
    const auto existing = hard_links->find(key);
    if (existing != hard_links->end()) {
      std::error_code error;
      std::filesystem::create_hard_link(existing->second, destination, error);
      if (error) {
        return FilesystemError(
            error, fmt::format("link {} to {}", existing->second.string(),
                               destination.string()));
      }
      return absl::OkStatus();
    }
  }
  PUU_RETURN_IF_ERROR(CopyFileContents(source, destination));
  PUU_RETURN_IF_ERROR(
      SetPermissions(destination, PermissionBits(info.st_mode)));
  PUU_RETURN_IF_ERROR(CopyXattrs(source, destination));
  if (info.st_nlink > 1) hard_links->emplace(key, destination);
  return absl::OkStatus();
}

Status CopyTreeEntry(const std::filesystem::path& source,
                     const std::filesystem::path& destination,
                     HardLinkMap* hard_links) {
  struct stat info = {};
  if (::lstat(source.c_str(), &info) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("lstat {}", source.string()));
  }
  if (S_ISDIR(info.st_mode)) {
    return CopyDirectory(source, destination, info, hard_links);
  }
  if (S_ISLNK(info.st_mode)) return CopySymlink(source, destination);
  if (S_ISREG(info.st_mode)) {
    return CopyRegularFile(source, destination, info, hard_links);
  }

  return InvalidArgumentError("unsupported file type at {}", source.string());
}

Status CopyTreeFilesEntries(const std::filesystem::path& source,
                            const std::filesystem::path& destination) {
  PUU_RETURN_IF_ERROR(CreateDirectories(destination));
  std::error_code error;
  std::filesystem::directory_iterator entries(source, error);
  if (error) {
    return FilesystemError(error, fmt::format("readdir {}", source.string()));
  }
  const std::filesystem::directory_iterator end;
  for (; entries != end; entries.increment(error)) {
    if (error) {
      return FilesystemError(error, fmt::format("readdir {}", source.string()));
    }
    const std::filesystem::path child = entries->path();
    const std::filesystem::path target = destination / child.filename();

    struct stat info = {};
    if (::lstat(child.c_str(), &info) != 0) {
      const int errno_value = errno;
      return ErrnoToStatus(errno_value,
                           fmt::format("lstat {}", child.string()));
    }
    if (S_ISDIR(info.st_mode)) {
      PUU_RETURN_IF_ERROR(CopyTreeFilesEntries(child, target));
      continue;
    }
    PUU_RETURN_IF_ERROR(CopyFileContents(child, target));
  }
  return absl::OkStatus();
}

}  // namespace

std::shared_ptr<spdlog::logger> InstallationLogger() {
  const std::shared_ptr<spdlog::logger> logger =
      spdlog::get(std::string(kInstallationLogTarget));
  return logger ? logger : spdlog::default_logger();
}

Status EnterPrivateMountNamespace() {
  if (::unshare(CLONE_NEWNS) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, "unshare(CLONE_NEWNS)");
  }

  // namespace, which is exactly what the new namespace is meant to prevent.
  if (::mount(nullptr, "/", nullptr, MS_PRIVATE | MS_REC, nullptr) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, "mount / as private");
  }
  return absl::OkStatus();
}

Status MountTarget(const std::filesystem::path& source,
                   const std::filesystem::path& target,
                   std::string_view filesystem_type, std::string_view options) {
  PUU_RETURN_IF_ERROR(CreateDirectories(target));
  PUU_ASSIGN_OR_RETURN(const Mounts mounts, Mounts::Read());
  if (mounts.IsMountPoint(target)) {
    InstallationLogger()->info("{} is already mounted; reusing it",
                               target.string());
    return absl::OkStatus();
  }
  const std::string type(filesystem_type);
  const std::string data(options);
  if (::mount(source.c_str(), target.c_str(), type.c_str(), 0,
              data.empty() ? nullptr : data.c_str()) != 0) {
    const int error = errno;
    return ErrnoToStatus(
        error, fmt::format("mount {} at {}", source.string(), target.string()));
  }
  return absl::OkStatus();
}

Status BindMount(const std::filesystem::path& source,
                 const std::filesystem::path& target) {
  if (::mount(source.c_str(), target.c_str(), nullptr, MS_BIND | MS_REC,
              nullptr) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("bind-mount {} at {}",
                                            source.string(), target.string()));
  }
  return absl::OkStatus();
}

Status Unmount(const std::filesystem::path& target) {
  if (::umount2(target.c_str(), 0) == 0) {
    return absl::OkStatus();
  }
  int error = errno;

  if (error == EINVAL || error == ENOENT) {
    return absl::OkStatus();
  }
  if (error == EBUSY) {
    InstallationLogger()->warn("{} is busy, detaching lazily", target.string());
    if (::umount2(target.c_str(), MNT_DETACH) == 0) {
      return absl::OkStatus();
    }
    error = errno;
  }
  return ErrnoToStatus(error, fmt::format("unmount {}", target.string()));
}

Status MountChrootBinds() {
  PUU_ASSIGN_OR_RETURN(const Mounts mounts, Mounts::Read());
  for (const auto& [source, relative] : kChrootBindMounts) {
    const std::filesystem::path target =
        std::filesystem::path(kTargetMount) / relative;
    PUU_RETURN_IF_ERROR(CreateDirectories(target));
    if (mounts.IsMountPoint(target)) continue;
    PUU_RETURN_IF_ERROR(BindMount(std::filesystem::path(source), target));
  }
  return absl::OkStatus();
}

Status CleanupMounts() {
  PUU_ASSIGN_OR_RETURN(const std::vector<std::filesystem::path> points,
                       TeardownMountPoints());
  Status first_error;
  for (const std::filesystem::path& point : points) {
    InstallationLogger()->info("unmounting {}", point.string());
    const Status status = Unmount(point);
    if (!status.ok() && first_error.ok()) first_error = status;
  }
  return first_error;
}

void Rollback() {
  try {
    const StatusOr<std::vector<std::filesystem::path>> points =
        TeardownMountPoints();
    if (!points.ok()) return;
    for (const std::filesystem::path& point : *points) {
      InstallationLogger()->info("rollback: unmounting {}", point.string());

      Unmount(point).IgnoreError();
    }
  } catch (const std::exception& error) {
    InstallationLogger()->error("rollback failed: {}", error.what());
  }
}

CleanupStack::~CleanupStack() {
  while (!cleanups_.empty()) {
    const std::function<void()> cleanup = std::move(cleanups_.back());
    cleanups_.pop_back();

    try {
      cleanup();
    } catch (const std::exception& error) {
      InstallationLogger()->error("cleanup failed: {}", error.what());
    } catch (...) {
      InstallationLogger()->error("cleanup failed");
    }
  }
}

void CleanupStack::Push(std::function<void()> cleanup) {
  cleanups_.push_back(std::move(cleanup));
}

void CleanupStack::Release() { cleanups_.clear(); }

std::string MergeFstab(std::string_view existing, std::string_view efi_partuuid,
                       std::string_view home_uuid) {
  constexpr std::array<std::string_view, 2> kManaged = {"/efi", "/var/home"};
  std::string merged;
  while (!existing.empty()) {
    const size_t newline = existing.find('\n');
    std::string_view line = existing.substr(0, newline);
    existing = newline == std::string_view::npos ? std::string_view()
                                                 : existing.substr(newline + 1);
    if (line.ends_with('\r')) line.remove_suffix(1);
    if (std::find(kManaged.begin(), kManaged.end(), FstabMountPoint(line)) !=
        kManaged.end()) {
      continue;
    }
    merged.append(line).push_back('\n');
  }
  merged += fmt::format(
      "PARTUUID={} /efi vfat fmask=0077,dmask=0077,nofail 0 2\n", efi_partuuid);
  merged += fmt::format("UUID={} /var/home ext4 defaults 0 2\n", home_uuid);
  return merged;
}

StatusOr<std::vector<KernelArg>> InstallKernelArgs(std::string_view root_uuid,
                                                   const Config& config) {
  PUU_ASSIGN_OR_RETURN(const Arch machine, HostArch());
  std::vector<KernelArg> args;
  args.reserve(kFixedKernelArgs.size() + 2);
  PUU_ASSIGN_OR_RETURN(
      KernelArg root, KernelArg::Parse(fmt::format("root=UUID={}", root_uuid)));
  args.push_back(std::move(root));
  for (const std::string_view text : kFixedKernelArgs) {
    PUU_ASSIGN_OR_RETURN(KernelArg arg, KernelArg::Parse(text));
    args.push_back(std::move(arg));
  }

  const auto configured =
      config.serial_console.find(std::string(ArchName(machine)));
  if (configured != config.serial_console.end()) {
    args.push_back(configured->second);
    return args;
  }

  const std::string_view fallback = machine == Arch::kX86_64
                                        ? "console=ttyS0,115200"
                                        : "console=ttyAMA0,115200";
  PUU_ASSIGN_OR_RETURN(KernelArg console, KernelArg::Parse(fallback));
  args.push_back(std::move(console));
  return args;
}

StatusOr<std::filesystem::path> DeploymentRoot(
    const std::filesystem::path& target) {
  const std::filesystem::path deploy = target / "ostree/deploy";
  std::error_code error;
  std::filesystem::directory_iterator os_entries(deploy, error);
  if (error) {
    return FilesystemError(
        error, fmt::format("read ostree deployments at {}", deploy.string()));
  }

  std::vector<std::filesystem::path> found;
  const std::filesystem::directory_iterator end;
  for (; os_entries != end; os_entries.increment(error)) {
    if (error) {
      return FilesystemError(
          error, fmt::format("read ostree deployments at {}", deploy.string()));
    }

    std::error_code skipped;
    std::filesystem::directory_iterator deployments(
        os_entries->path() / "deploy", skipped);
    if (skipped) continue;
    for (; deployments != end && !skipped; deployments.increment(skipped)) {
      const std::filesystem::path path = deployments->path();
      std::error_code kind_error;
      if (!std::filesystem::is_directory(path, kind_error)) continue;

      if (path.filename().string().ends_with(".origin")) continue;
      found.push_back(path);
    }
  }
  if (found.empty()) {
    return NotFoundError(
        "ostree did not report the installed deployment root under {}",
        deploy.string());
  }
  std::sort(found.begin(), found.end());
  return found.front();
}

StatusOr<std::vector<std::string>> FlatpakManifestRefs(
    const std::filesystem::path& manifest) {
  std::error_code error;
  if (!std::filesystem::exists(manifest, error)) {
    return std::vector<std::string>();
  }

  std::ifstream file(manifest);
  if (!file.is_open()) {
    const int failure = errno;
    return ErrnoToStatus(failure, fmt::format("open {}", manifest.string()));
  }
  std::vector<std::string> refs;
  std::string line;
  while (std::getline(file, line)) {
    std::string_view value(line);
    const size_t comment = value.find('#');
    if (comment != std::string_view::npos) value = value.substr(0, comment);
    const size_t start = value.find_first_not_of(" \t\r\n");
    if (start == std::string_view::npos) continue;
    const size_t last = value.find_last_not_of(" \t\r\n");
    refs.emplace_back(value.substr(start, last - start + 1));
  }
  if (file.bad()) {
    return InternalError("failed to read {}", manifest.string());
  }
  return refs;
}

Status CopyTree(const std::filesystem::path& source,
                const std::filesystem::path& destination) {
  PUU_RETURN_IF_ERROR(RemoveTree(destination));
  HardLinkMap hard_links;
  return CopyTreeEntry(source, destination, &hard_links);
}

Status CopyTreeFiles(const std::filesystem::path& source,
                     const std::filesystem::path& destination) {
  PUU_RETURN_IF_ERROR(RemoveTree(destination));
  return CopyTreeFilesEntries(source, destination);
}

}  // namespace puu::engine
