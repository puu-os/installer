// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_PIPELINE_H_
#define PUU_ENGINE_PIPELINE_H_

#include <filesystem>
#include <functional>
#include <memory>
#include <string>
#include <string_view>
#include <vector>

#include <spdlog/logger.h>

#include "base/status.h"
#include "config.h"

namespace puu::engine {

// Log target for messages that belong on the installer's progress screen. Other
// log records go to the file only.
inline constexpr std::string_view kInstallationLogTarget = "installation";

// The logger for that target, falling back to the default logger before
// logging is configured.
std::shared_ptr<spdlog::logger> InstallationLogger();

// Mount points owned by the installer. All live under /run so that nothing
// survives a reboot.
inline constexpr std::string_view kTargetMount = "/run/puu-installer/target";
inline constexpr std::string_view kTargetEfiMount =
    "/run/puu-installer/target/efi";
inline constexpr std::string_view kTargetHomeMount =
    "/run/puu-installer/target/var/home";

// Puts the process in a private mount namespace.
//
// Must be called before any mount. Every mount the installer makes then
// disappears when it exits, and the /var/tmp bind used by bootc never becomes
// visible to the rest of the system.
Status EnterPrivateMountNamespace();

// Mounts `source` on `target`, creating the mount point. Succeeds without
// acting when `target` is already a mount point.
Status MountTarget(const std::filesystem::path& source,
                   const std::filesystem::path& target,
                   std::string_view filesystem_type, std::string_view options);

Status BindMount(const std::filesystem::path& source,
                 const std::filesystem::path& target);

Status Unmount(const std::filesystem::path& target);

// Bind-mounts /proc, /sys, /dev and /run under `root`, so helpers can be run
// chrooted there.
//
// `root` is the OSTree deployment directory, not the physical sysroot: only the
// deployment has a /usr for the helper to execute from.
Status MountChrootBinds(const std::filesystem::path& root);

// Unmounts everything the installer mounted, deepest path first.
Status CleanupMounts();

// Best-effort unwind after a failure or cancellation.
//
// This only unmounts. A partition table that has already been written is not
// restored, because there is nothing to restore it from.
void Rollback();

// Runs registered cleanup callbacks in reverse order when destroyed, including
// during stack unwinding.
class CleanupStack {
 public:
  CleanupStack() = default;
  ~CleanupStack();

  CleanupStack(const CleanupStack&) = delete;
  CleanupStack& operator=(const CleanupStack&) = delete;

  void Push(std::function<void()> cleanup);

  // Abandons all registered cleanups, for the success path.
  void Release();

 private:
  std::vector<std::function<void()>> cleanups_;
};

// Merges the installer's entries into an existing fstab.
//
// Any previous /efi or /var/home line is dropped first, so re-running does not
// accumulate duplicates. The ESP is referenced by PARTUUID and home by
// filesystem UUID.
std::string MergeFstab(std::string_view existing, std::string_view efi_partuuid,
                       std::string_view home_uuid);

StatusOr<std::vector<KernelArg>> InstallKernelArgs(std::string_view root_uuid,
                                                   const Config& config);

// Locates the ostree deployment directory below the target root.
StatusOr<std::filesystem::path> DeploymentRoot(
    const std::filesystem::path& target);

}  // namespace puu::engine

#endif  // PUU_ENGINE_PIPELINE_H_
