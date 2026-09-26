// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include <fcntl.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <cerrno>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <absl/status/status.h>
#include <absl/strings/str_join.h>
#include <fmt/format.h>
#include <nlohmann/json.hpp>
#include <openssl/crypto.h>

#include "base/status.h"
#include "config.h"
#include "engine/block.h"
#include "engine/command.h"
#include "engine/context.h"
#include "engine/firmware.h"
#include "engine/image.h"
#include "engine/partition.h"
#include "engine/passwd.h"
#include "engine/pipeline.h"
#include "engine/progress.h"
#include "engine/secureboot.h"
#include "engine/step.h"
#include "engine/util.h"

namespace puu::engine {
namespace {

constexpr std::array<std::string_view, 4> kDependencies = {
    "bootc", "mke2fs", "mkfs.fat", "mkfs.xfs"};

constexpr std::chrono::seconds kDeviceTimeout{30};
constexpr std::chrono::seconds kHeartbeatInterval{30};

constexpr mode_t kPublicFileMode = 0644;
constexpr mode_t kSecretFileMode = 0600;

constexpr std::string_view kBootcStorageConf =
    "/run/puu-installer/bootc-storage.conf";
constexpr std::string_view kBootcGraphRoot =
    "/run/puu-installer/target/var/tmp/puu-bootc-storage/containers/storage";
constexpr std::string_view kContainersRunRoot = "/run/containers/storage";
constexpr std::string_view kHostVarTmp = "/var/tmp";

std::filesystem::path InTarget(const std::filesystem::path& path) {
  return std::filesystem::path(kTargetMount) / path.relative_path();
}

StatusOr<std::filesystem::path> StateRoot() {
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path deployment,
                       DeploymentRoot(kTargetMount));
  std::filesystem::path state = deployment.parent_path().parent_path();
  if (state.empty() || state == deployment) {
    return InternalError("deployment root has unexpected path: {}",
                         deployment.string());
  }
  return state;
}

Status MountEfi(const Runner& runner) {
  return MountTarget(runner.GetContext().parts.efi_part, kTargetEfiMount,
                     "vfat", "fmask=0077,dmask=0077");
}

StatusOr<uint64_t> ValidateTargetDrive(const std::filesystem::path& drive,
                                       const Config& config) {
  std::error_code error;
  const std::filesystem::file_status status =
      std::filesystem::status(drive, error);
  if (error) {
    return FilesystemError(
        error, fmt::format("inspect target drive {}", drive.string()));
  }
  if (!std::filesystem::is_block_file(status)) {
    return InvalidArgumentError("'{}' is not a block device", drive.string());
  }
  PUU_ASSIGN_OR_RETURN(const BusySources busy_sources, ReadBusySources());
  PUU_ASSIGN_OR_RETURN(const std::optional<BusyReason> busy,
                       DiskBusy(drive, busy_sources));
  if (busy.has_value()) {
    return FailedPreconditionError("'{}' is busy: {}", drive.string(),
                                   BusyReasonText(*busy));
  }
  PUU_ASSIGN_OR_RETURN(const uint64_t size, DriveSizeBytes(drive));
  PUU_ASSIGN_OR_RETURN(const uint64_t minimum, config.MinTargetSizeBytes());
  if (size < minimum) {
    return FailedPreconditionError(
        "'{}' is too small: {} detected, need at least {} for EFI, root and "
        "home partitions",
        drive.string(), HumanSize(size), HumanSize(minimum));
  }
  return size;
}

// Confirms the disk behind `context.drive` is still the one the operator
// selected, then returns its size.
//
// The first call records the identity; later calls compare against it. A drive
// udev cannot identify is matched on size alone, which is weaker but still
// catches the common case of a swapped removable disk.
StatusOr<uint64_t> ValidateSelectedDrive(Context& context) {
  PUU_ASSIGN_OR_RETURN(const uint64_t size,
                       ValidateTargetDrive(context.drive, context.config));
  PUU_ASSIGN_OR_RETURN(const std::string id, DriveIdentity(context.drive));

  if (context.drive_size == 0) {
    context.drive_id = id;
    context.drive_size = size;
    return size;
  }
  if (id != context.drive_id || size != context.drive_size) {
    return FailedPreconditionError(
        "'{}' is no longer the disk that was selected; refusing to erase it. "
        "Restart the installer and choose the target again",
        context.drive.string());
  }
  return size;
}

StatusOr<ImageRef> SourceImageRef(const std::filesystem::path& source) {
  const std::string_view transport =
      IsDirectory(source) ? "dir" : "oci-archive";
  return ImageRef::Parse(fmt::format("{}:{}", transport, source.string()));
}

// Resolves an absolute in-target path against the OSTree deployment root.
//
// The physical sysroot has no /usr or /etc of its own; both live in the
// deployment, so anything a chrooted helper will see must be resolved here.
StatusOr<std::filesystem::path> InDeployment(
    const std::filesystem::path& path) {
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path deployment,
                       DeploymentRoot(kTargetMount));
  return deployment / path.relative_path();
}

Status RunChrootedHelper(const Runner& runner,
                         const std::filesystem::path& helper,
                         std::string heartbeat) {
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path deployment,
                       DeploymentRoot(kTargetMount));
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path state, StateRoot());

  CleanupStack binds;
  binds.Push([deployment] {
    for (const std::string_view relative :
         {"run", "dev", "sys", "proc", "var"}) {
      Unmount(deployment / relative).IgnoreError();
    }
  });

  // Without this the helper writes into the deployment's empty /var, which
  // ostree-prepare-root shadows with the stateroot's /var at boot, so every
  // change would be silently discarded.
  const std::filesystem::path var = deployment / "var";
  PUU_RETURN_IF_ERROR(CreateDirectories(var));
  PUU_RETURN_IF_ERROR(BindMount(state / "var", var));
  PUU_RETURN_IF_ERROR(MountChrootBinds(deployment));

  MarkerProgress progress;
  Command command(helper.string());
  command.Chroot(deployment)
      .Heartbeat(kHeartbeatInterval, std::move(heartbeat))
      .Track(&progress,
             [&runner](double fraction) { runner.ReportProgress(fraction); });
  return command.Run();
}

Status SeedStore(const Runner& runner, const std::filesystem::path& live_store,
                 const std::filesystem::path& sync_helper,
                 const std::filesystem::path& relative_target,
                 std::string_view what) {
  if (IsDirectory(live_store)) {
    PUU_ASSIGN_OR_RETURN(const std::filesystem::path state, StateRoot());
    PUU_RETURN_IF_ERROR(CopyTree(live_store, state / relative_target));
    InstallationLogger()->info("{}: copied the store from the live image",
                               what);
    return absl::OkStatus();
  }
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path helper,
                       InDeployment(sync_helper));
  if (!IsRegularFile(helper)) {
    InstallationLogger()->info("{}: nothing to pre-stage", what);
    return absl::OkStatus();
  }
  InstallationLogger()->info("{}: running {}", what, sync_helper.string());
  PUU_RETURN_IF_ERROR(RunChrootedHelper(
      runner, sync_helper,
      fmt::format("{} pre-seed still running; downloading content", what)));
  InstallationLogger()->info("{}: pre-staged", what);
  return absl::OkStatus();
}

StatusOr<std::filesystem::path> SystemdBootSource() {
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path deployment,
                       DeploymentRoot(kTargetMount));
  const std::array<std::filesystem::path, 2> directories = {
      deployment / "usr/lib/systemd/boot/efi",
      std::filesystem::path("/usr/lib/systemd/boot/efi")};
  for (const std::filesystem::path& directory : directories) {
    std::error_code error;
    std::filesystem::directory_iterator entries(directory, error);
    if (error) {
      continue;
    }
    const std::filesystem::directory_iterator end;
    for (; entries != end && !error; entries.increment(error)) {
      std::filesystem::path path = entries->path();
      const std::string name = path.filename().string();
      std::string extension = path.extension().string();
      std::ranges::transform(extension, extension.begin(),
                             [](unsigned char c) { return std::tolower(c); });
      if (name.starts_with("systemd-boot") && extension == ".efi") {
        return path;
      }
    }
  }
  return NotFoundError("systemd-boot EFI binary not found");
}

Status StepDependencies(Runner& /*runner*/) {
  std::vector<std::string> missing;
  for (const std::string_view dependency : kDependencies) {
    if (!FindInPath(dependency).has_value()) {
      missing.emplace_back(dependency.data(), dependency.size());
    }
  }
  if (!missing.empty()) {
    for (const std::string_view dependency : missing) {
      InstallationLogger()->info("missing required tool: {}", dependency);
    }
    return FailedPreconditionError("missing dependencies: {}",
                                   absl::StrJoin(missing, ", "));
  }
  InstallationLogger()->info("all {} dependencies present",
                             kDependencies.size());
  return absl::OkStatus();
}

Status StepSetupMode(Runner& runner) {
  if (runner.GetContext().secure_boot == SecureBootMode::kSkip) {
    InstallationLogger()->info(
        "Secure Boot disabled; skipping firmware setup check");
    return absl::OkStatus();
  }
  const SetupMode mode = DetectSetupMode();
  switch (mode.state) {
    case SetupMode::State::kUnknown:

      runner.GetContext().secure_boot = SecureBootMode::kSkip;
      InstallationLogger()->info(
          "Secure Boot Setup Mode could not be detected; continuing without "
          "Secure Boot");
      if (!mode.reason.empty()) {
        InstallationLogger()->info("  reason: {}", mode.reason);
      }
      return absl::OkStatus();
    case SetupMode::State::kOff:
      return FailedPreconditionError(
          "Secure Boot Setup Mode is off. Reboot to firmware setup or "
          "continue without Secure Boot.");
    case SetupMode::State::kOn:
      InstallationLogger()->info(
          "firmware reports Setup Mode on; Secure Boot is {}",
          mode.secure_boot_enabled.value_or(false) ? "enabled" : "disabled");
      return absl::OkStatus();
  }
  return InternalError("unhandled Secure Boot setup mode");
}

Status StepCleanup(Runner& /*runner*/) { return CleanupMounts(); }

Status StepValidateDrive(Runner& runner) {
  Context& context = runner.GetContext();
  PUU_ASSIGN_OR_RETURN(const uint64_t size, ValidateSelectedDrive(context));
  if (context.source_image.has_value() &&
      !IsRegularFile(*context.source_image) &&
      !IsDirectory(*context.source_image)) {
    return NotFoundError("source bootc image not found at {}",
                         context.source_image->string());
  }

  context.parts.efi_part = PartitionPath(context.drive, kEfiPartitionNumber);
  context.parts.root_part = PartitionPath(context.drive, kRootPartitionNumber);
  context.parts.home_part = PartitionPath(context.drive, kHomePartitionNumber);

  InstallationLogger()->info(
      "target {} ({}; {}, {}, {})", context.drive.string(), HumanSize(size),
      context.parts.efi_part.string(), context.parts.root_part.string(),
      context.parts.home_part.string());
  return absl::OkStatus();
}

Status StepVerifyImage(Runner& runner) {
  const Context& context = runner.GetContext();
  if (!context.source_image.has_value()) {
    InstallationLogger()->info("using the running bootc image as the source");
    return absl::OkStatus();
  }
  const std::optional<Digest>& expected = context.config.image.expected_digest;
  if (!expected.has_value()) {
    return InvalidArgumentError(
        "image.expectedDigest is required when installing from a local image "
        "source");
  }
  InstallationLogger()->info("expected image digest: {}", expected->Value());
  PUU_RETURN_IF_ERROR(VerifyImageDigest(*context.source_image,
                                        context.image_manifest, *expected));
  InstallationLogger()->info("image digest verified: {}", expected->Value());
  return absl::OkStatus();
}

Status StepPartition(Runner& runner) {
  Context& context = runner.GetContext();

  PUU_RETURN_IF_ERROR(ValidateSelectedDrive(context).status());
  PUU_ASSIGN_OR_RETURN(context.parts.layout,
                       PartitionDrive(context.drive, context.config));

  PUU_RETURN_IF_ERROR(WaitForUdevSettle(kDeviceTimeout));
  for (const std::filesystem::path* part :
       {&context.parts.efi_part, &context.parts.root_part,
        &context.parts.home_part}) {
    PUU_RETURN_IF_ERROR(WaitForBlockDevice(*part, kDeviceTimeout));
  }
  return absl::OkStatus();
}

Status StepFormatEfi(Runner& runner) {
  Context& context = runner.GetContext();
  const std::filesystem::path part = context.parts.efi_part;
  InstallationLogger()->info("formatting {} as FAT32 (label EFI)",
                             part.string());
  Command command("mkfs.fat");
  PUU_RETURN_IF_ERROR(command.Args({"-F32", "-n", "EFI", part.string()}).Run());

  PUU_ASSIGN_OR_RETURN(context.parts.efi_uuid, CapturePartUuid(part));
  return absl::OkStatus();
}

Status StepFormatRoot(Runner& runner) {
  Context& context = runner.GetContext();
  const std::filesystem::path part = context.parts.root_part;
  const std::string label = context.config.labels.root.Value();
  InstallationLogger()->info("formatting {} as XFS (label {})", part.string(),
                             label);
  Command command("mkfs.xfs");
  PUU_RETURN_IF_ERROR(command.Args({"-f", "-L", label, part.string()}).Run());
  PUU_ASSIGN_OR_RETURN(context.parts.root_uuid, CaptureUuid(part));
  return absl::OkStatus();
}

Status StepFormatHome(Runner& runner) {
  Context& context = runner.GetContext();
  const std::filesystem::path part = context.parts.home_part;
  const std::string label = context.config.labels.home.Value();
  InstallationLogger()->info("formatting {} as ext4 (label {})", part.string(),
                             label);
  Command command("mke2fs");

  PUU_RETURN_IF_ERROR(command
                          .Args({"-t", "ext4", "-O", "encrypt", "-F", "-L",
                                 label, part.string()})
                          .Run());
  PUU_ASSIGN_OR_RETURN(context.parts.home_uuid, CaptureUuid(part));
  return absl::OkStatus();
}

Status StepMount(Runner& runner) {
  return MountTarget(runner.GetContext().parts.root_part, kTargetMount, "xfs",
                     "");
}

Status StepBootcInstall(Runner& runner) {
  Context& context = runner.GetContext();
  if (context.parts.root_uuid.empty()) {
    return FailedPreconditionError(
        "root partition UUID is unset; the root filesystem was not formatted");
  }
  if (!context.source_image.has_value() && !RunningInContainer()) {
    return FailedPreconditionError(
        "no local bootc source image found; expected {}/image.tar",
        context.config.image.source_mount.string());
  }

  CleanupStack cleanups;
  const std::filesystem::path scratch = InTarget("/var/tmp");
  PUU_RETURN_IF_ERROR(CreateDirectories(scratch));
  PUU_RETURN_IF_ERROR(BindMount(scratch, scratch));
  cleanups.Push([scratch] {
    Unmount(scratch).IgnoreError();
    RemoveTree(scratch).IgnoreError();
  });
  PUU_RETURN_IF_ERROR(BindMount(scratch, kHostVarTmp));
  cleanups.Push([] { Unmount(kHostVarTmp).IgnoreError(); });
  PUU_RETURN_IF_ERROR(CreateDirectories(kBootcGraphRoot));
  PUU_RETURN_IF_ERROR(CreateDirectories(kContainersRunRoot));
  PUU_RETURN_IF_ERROR(WriteFile(
      kBootcStorageConf,
      fmt::format("[storage]\ndriver = \"overlay\"\ngraphroot = \"{}\"\n"
                  "runroot = \"{}\"\n",
                  kBootcGraphRoot, kContainersRunRoot),
      kPublicFileMode));

  std::vector<std::string> args = {"install", "to-filesystem"};
  if (context.source_image.has_value()) {
    ImageRef source;
    if (context.image_manifest.source_imgref.has_value()) {
      source = *context.image_manifest.source_imgref;
    } else {
      PUU_ASSIGN_OR_RETURN(source, SourceImageRef(*context.source_image));
    }
    args.emplace_back("--source-imgref");
    args.push_back(source.Value());
  }
  args.emplace_back("--target-imgref");
  args.push_back(context.target_imgref.Value());
  args.emplace_back("--skip-fetch-check");
  args.emplace_back("--bootloader");
  args.emplace_back("none");
  args.emplace_back("--skip-finalize");
  PUU_ASSIGN_OR_RETURN(
      const std::vector<KernelArg> kernel_args,
      InstallKernelArgs(context.parts.root_uuid, context.config));
  for (const KernelArg& argument : kernel_args) {
    args.emplace_back("--karg");
    args.push_back(argument.Value());
  }
  args.emplace_back(kTargetMount);

  InstallationLogger()->info(
      "bootc image deployment may take several minutes without native "
      "progress output");

  Command command("bootc");
  command.Args(args)
      .NewPidNamespace()
      .Env("CONTAINERS_STORAGE_CONF", std::string(kBootcStorageConf))
      .Env("TMPDIR", std::string(kHostVarTmp))
      .Heartbeat(kHeartbeatInterval,
                 "bootc install still running; deploying image");

  std::optional<DiskWriteProgress> progress;
  if (context.source_image.has_value()) {
    const StatusOr<uint64_t> expected =
        EstimatedInstallBytes(*context.source_image, context.image_manifest);
    if (expected.ok()) {
      std::error_code error;
      const uintmax_t archive_bytes =
          std::filesystem::file_size(*context.source_image, error);
      progress.emplace(kTargetMount,
                       *expected + (error ? 0 : uint64_t{archive_bytes}));
      command.Track(&progress.value(), [&runner](double fraction) {
        runner.ReportProgress(fraction);
      });
    }
  }
  return command.Run();
}

Status StepMountHome(Runner& runner) {
  return MountTarget(runner.GetContext().parts.home_part, kTargetHomeMount,
                     "ext4", "");
}

Status StepConfigureMounts(Runner& runner) {
  const Context& context = runner.GetContext();
  if (context.parts.efi_uuid.empty() || context.parts.home_uuid.empty()) {
    return FailedPreconditionError(
        "EFI or home partition UUID is unset; the filesystems were not "
        "formatted");
  }

  PUU_ASSIGN_OR_RETURN(const std::filesystem::path deployment,
                       DeploymentRoot(kTargetMount));

  for (const std::string_view relative : {"efi", "var", "var/home", "etc"}) {
    PUU_RETURN_IF_ERROR(CreateDirectories(deployment / relative));
  }

  const std::filesystem::path fstab = deployment / "etc/fstab";
  std::string existing;
  if (Exists(fstab)) {
    PUU_ASSIGN_OR_RETURN(existing, ReadFile(fstab));
  }
  return WriteFile(
      fstab,
      MergeFstab(existing, context.parts.efi_uuid, context.parts.home_uuid),
      kPublicFileMode);
}

Status StepFirstboot(Runner& runner) {
  const IdentityConfig& identity = runner.GetContext().config.identity;
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path deployment,
                       DeploymentRoot(kTargetMount));

  if (!IsRegularFile(deployment / "usr/share/zoneinfo" /
                     identity.timezone.Value())) {
    return NotFoundError("timezone not found in target system: {}",
                         identity.timezone.Value());
  }

  const std::filesystem::path etc = deployment / "etc";
  PUU_RETURN_IF_ERROR(CreateDirectories(etc));

  // These overwrite whatever the image ships. Image defaults are placeholders;
  // preserving them would silently discard the operator's choice and leave
  // every machine on the same hostname.
  PUU_RETURN_IF_ERROR(WriteFile(etc / "hostname",
                                fmt::format("{}\n", identity.hostname.Value()),
                                kPublicFileMode));
  InstallationLogger()->info("staged hostname {} in /etc/hostname",
                             identity.hostname.Value());
  if (!identity.locale.Empty()) {
    PUU_RETURN_IF_ERROR(WriteFile(
        etc / "locale.conf", fmt::format("LANG={}\n", identity.locale.Value()),
        kPublicFileMode));
    InstallationLogger()->info("staged locale {} in /etc/locale.conf",
                               identity.locale.Value());
  }

  const std::filesystem::path localtime = etc / "localtime";
  PUU_RETURN_IF_ERROR(RemoveFile(localtime));
  PUU_RETURN_IF_ERROR(CreateSymlink(
      std::filesystem::path("../usr/share/zoneinfo") /
          identity.timezone.Value(),
      localtime));

  if (!identity.keymap.Empty()) {
    PUU_RETURN_IF_ERROR(WriteFile(
        etc / "vconsole.conf",
        fmt::format("KEYMAP={}\n", identity.keymap.Value()), kPublicFileMode));
    InstallationLogger()->info("staged keymap {} in /etc/vconsole.conf",
                               identity.keymap.Value());
  }
  return absl::OkStatus();
}

Status StepConfigureK3s(Runner& runner) {
  Context& context = runner.GetContext();
  const K3sConfig& k3s = context.config.k3s;

  if (k3s.role == K3sRole::kServer && !k3s.node_name.has_value()) {
    InstallationLogger()->info(
        "k3s cluster: using image default server configuration");
    return absl::OkStatus();
  }

  std::vector<std::string> args = {"--role",
                                   std::string(K3sRoleName(k3s.role))};
  if (k3s.server_url.has_value()) {
    args.emplace_back("--server-url");
    args.push_back(k3s.server_url->Value());
  }

  if (k3s.node_name.has_value()) {
    args.emplace_back("--node-name");
    args.push_back(k3s.node_name->Value());
  }

  PUU_ASSIGN_OR_RETURN(const std::filesystem::path deployment,
                       DeploymentRoot(kTargetMount));
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path state, StateRoot());
  const std::filesystem::path deployment_var = deployment / "var";

  Status result;
  const auto record_cleanup = [&result](const Status& status) {
    if (!status.ok()) {
      InstallationLogger()->error("k3s cluster cleanup: {}", status.ToString());
      result.Update(status);
    }
  };
  {
    CleanupStack cleanups;
    result = [&]() -> Status {
      PUU_RETURN_IF_ERROR(CreateDirectories(deployment_var));
      PUU_RETURN_IF_ERROR(BindMount(state / "var", deployment_var));
      cleanups.Push([&record_cleanup, deployment_var] {
        record_cleanup(Unmount(deployment_var));
      });

      if (!context.k3s_token.Empty()) {
        std::string directory = (state / "var/.puu-k3s-XXXXXX").string();
        if (::mkdtemp(directory.data()) == nullptr) {
          return ErrnoToStatus(errno, "create k3s token directory");
        }
        cleanups.Push([&record_cleanup, directory] {
          record_cleanup(RemoveTree(directory));
        });

        const std::filesystem::path token =
            std::filesystem::path(directory) / "token";
        std::string content;
        content.reserve(context.k3s_token.Size() + 1);
        content.assign(context.k3s_token.Value());
        content.push_back('\n');
        const Status written =
            WriteFile(token, content, kSecretFileMode, O_NOFOLLOW | O_EXCL);
        OPENSSL_cleanse(content.data(), content.size());
        PUU_RETURN_IF_ERROR(written);
        args.emplace_back("--token-file");
        args.push_back(
            (std::filesystem::path("/") / token.lexically_relative(state))
                .string());
      }

      Command command(k3s.helper.string());
      return command.Args(args).Chroot(deployment).Run();
    }();
  }
  context.k3s_token.Clear();
  PUU_RETURN_IF_ERROR(result);
  InstallationLogger()->info("k3s cluster: staged {} configuration",
                             K3sRoleName(k3s.role));
  return absl::OkStatus();
}

Status StepStageHomedIdentity(Runner& runner) {
  Context& context = runner.GetContext();
  if (!context.username.has_value()) {
    return InvalidArgumentError(
        "username is required to stage a homed identity");
  }
  const std::string name = context.username->Value();
  const std::string_view password = context.user_password.Value();

  if (password.find_first_of("\r\n") != std::string_view::npos) {
    return InvalidArgumentError("password must not contain newline characters");
  }
  PUU_ASSIGN_OR_RETURN(const std::string hashed, HashPassword(password));

  std::vector<std::string> groups;
  groups.reserve(context.config.homed.groups.size());
  for (const GroupName& group : context.config.homed.groups) {
    groups.push_back(group.Value());
  }

  nlohmann::ordered_json identity;
  identity["userName"] = name;
  identity["secret"]["password"].push_back(std::string(password));
  identity["realName"] = name;
  identity["shell"] = std::string(UserShellPath(context.user_shell));
  identity["memberOf"] = groups;
  identity["homeDirectory"] = fmt::format("/var/home/{}", name);
  identity["imagePath"] = fmt::format("/var/home/{}.homedir", name);
  identity["storage"] = "fscrypt";
  identity["preferredSessionLauncher"] = "gnome";
  identity["enforcePasswordPolicy"] = false;
  identity["privileged"]["hashedPassword"].push_back(hashed);

  PUU_ASSIGN_OR_RETURN(const std::filesystem::path state, StateRoot());
  const std::filesystem::path target = state / "var/lib/puu/identity.json";
  PUU_RETURN_IF_ERROR(CreateDirectories(target.parent_path()));

  std::string text =
      identity.dump(2, ' ', false, nlohmann::json::error_handler_t::replace);
  const Status written = WriteFile(target, text, kSecretFileMode, O_NOFOLLOW);

  for (auto& value : identity["secret"]["password"]) {
    auto& held = value.get_ref<std::string&>();
    OPENSSL_cleanse(held.data(), held.size());
  }
  OPENSSL_cleanse(text.data(), text.size());
  identity = nlohmann::ordered_json();
  context.user_password.Clear();
  PUU_RETURN_IF_ERROR(written);

  InstallationLogger()->info(
      "staged homed identity for {} at /var/lib/puu/identity.json", name);
  return absl::OkStatus();
}

Status StepFlatpakSeed(Runner& runner) {
  const FlatpakConfig& flatpak = runner.GetContext().config.flatpak;
  if (!IsDirectory(flatpak.live_store)) {
    // The helper installs nothing when the manifest lists no refs, so skip the
    // chroot and the network round-trip entirely.
    PUU_ASSIGN_OR_RETURN(const std::filesystem::path manifest,
                         InDeployment(flatpak.manifest_path));
    PUU_ASSIGN_OR_RETURN(const std::vector<std::string> refs,
                         FlatpakManifestRefs(manifest));
    if (refs.empty()) {
      InstallationLogger()->info("flatpak-seed: no refs listed in {}",
                                 flatpak.manifest_path.string());
      return absl::OkStatus();
    }
  }
  return SeedStore(runner, flatpak.live_store, flatpak.sync_helper,
                   "var/lib/flatpak", "flatpak-seed");
}

Status StepModelsSeed(Runner& runner) {
  const ModelsConfig& models = runner.GetContext().config.models;
  return SeedStore(runner, models.live_store, models.sync_helper,
                   "var/lib/vllm/models", "models-seed");
}

Status StepInstallBootloader(Runner& runner) {
  PUU_RETURN_IF_ERROR(MountEfi(runner));
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path source, SystemdBootSource());
  PUU_ASSIGN_OR_RETURN(const Arch arch, HostArch());
  const std::string suffix(ArchEfiSuffix(arch));
  std::string upper = suffix;
  std::ranges::transform(upper, upper.begin(),
                         [](unsigned char c) { return std::toupper(c); });

  const std::filesystem::path systemd_dir =
      std::filesystem::path(kTargetEfiMount) / "EFI/systemd";
  const std::filesystem::path fallback_dir =
      std::filesystem::path(kTargetEfiMount) / "EFI/BOOT";
  PUU_RETURN_IF_ERROR(CreateDirectories(systemd_dir));
  PUU_RETURN_IF_ERROR(CreateDirectories(fallback_dir));

  PUU_RETURN_IF_ERROR(CopyFile(
      source, systemd_dir / fmt::format("systemd-boot{}.efi", suffix)));
  PUU_RETURN_IF_ERROR(
      CopyFile(source, fallback_dir / fmt::format("BOOT{}.EFI", upper)));
  InstallationLogger()->info("installed systemd-boot on the ESP");
  return absl::OkStatus();
}

Status StepStageEspBoot(Runner& runner) {
  PUU_RETURN_IF_ERROR(MountEfi(runner));
  const std::filesystem::path entries = InTarget("/boot/loader/entries");
  const std::filesystem::path ostree = InTarget("/boot/ostree");
  if (!IsDirectory(entries)) {
    return NotFoundError("boot loader entries not found: {}", entries.string());
  }
  if (!IsDirectory(ostree)) {
    return NotFoundError("boot ostree files not found: {}", ostree.string());
  }
  PUU_RETURN_IF_ERROR(CopyTreeFiles(
      entries, std::filesystem::path(kTargetEfiMount) / "loader/entries"));
  PUU_RETURN_IF_ERROR(CopyTreeFiles(
      ostree, std::filesystem::path(kTargetEfiMount) / "boot/ostree"));

  SyncAll();
  InstallationLogger()->info(
      "staged boot loader entries and ostree kernel files on ESP");
  return absl::OkStatus();
}

Status StepSign(Runner& /*runner*/) {
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path state, StateRoot());
  KeyStore store(state / "var");
  PUU_RETURN_IF_ERROR(store.EnsureKeys());
  PUU_ASSIGN_OR_RETURN(const KeyPair database, store.Get(KeyKind::kDatabase));
  PUU_ASSIGN_OR_RETURN(const std::vector<std::filesystem::path> binaries,
                       TargetEfiBinaries(kTargetEfiMount));
  if (binaries.empty()) {
    return NotFoundError("no EFI binaries found under {}", kTargetEfiMount);
  }

  size_t signed_count = 0;
  size_t skipped_count = 0;
  for (const std::filesystem::path& binary : binaries) {
    PUU_ASSIGN_OR_RETURN(const bool already, VerifyPeBinary(binary, database));
    if (already) {
      ++skipped_count;
      continue;
    }
    PUU_RETURN_IF_ERROR(SignPeBinary(binary, database));
    ++signed_count;
  }
  InstallationLogger()->info(
      "signed {} EFI binaries on target ESP ({} already signed)", signed_count,
      skipped_count);

  for (const std::filesystem::path& binary : binaries) {
    PUU_ASSIGN_OR_RETURN(const bool valid, VerifyPeBinary(binary, database));
    if (!valid) {
      return InternalError("signature verification failed for {}",
                           binary.string());
    }
  }
  return absl::OkStatus();
}

Status StepEnrollKeys(Runner& /*runner*/) {
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path state, StateRoot());
  KeyStore store(state / "var");
  PUU_RETURN_IF_ERROR(store.EnsureKeys());

  Status status = EnrollKeys(store);
  if (status.ok()) {
    InstallationLogger()->info("enrolled Secure Boot db, KEK and PK");
    return absl::OkStatus();
  }

  if (DetectSetupMode().state == SetupMode::State::kOff) {
    InstallationLogger()->info(
        "Secure Boot keys appear to be enrolled already");
    return absl::OkStatus();
  }
  return status;
}

Status StepNvram(Runner& runner) {
  PUU_RETURN_IF_ERROR(MountEfi(runner));
  Context& context = runner.GetContext();
  if (!context.parts.layout.has_value()) {
    PUU_ASSIGN_OR_RETURN(const EfiPartitionLayout layout,
                         LoadPartitionLayout(context.drive));
    context.parts.layout = layout;
  }
  PUU_ASSIGN_OR_RETURN(const Arch arch, HostArch());
  return ManageBootEntry(*context.parts.layout, arch,
                         context.config.branding.name.Value());
}

Status StepFinalCleanup(Runner& /*runner*/) {
  PUU_RETURN_IF_ERROR(CleanupMounts());
  InstallationLogger()->info("target filesystems unmounted; ready to reboot");
  return absl::OkStatus();
}

constexpr std::array<Step, 24> kSteps = {{
    {.title = "Verify dependencies", .run = &StepDependencies},
    {.title = "Verify Secure Boot setup",
     .run = &StepSetupMode,
     .requires_secure_boot = true},
    {.title = "Clean up previous mounts", .run = &StepCleanup},
    {.title = "Validate target drive", .run = &StepValidateDrive},
    {.title = "Verify image integrity", .run = &StepVerifyImage},
    {.title = "Partition drive",
     .run = &StepPartition,
     .point_of_no_return = true},
    {.title = "Format EFI system partition", .run = &StepFormatEfi},
    {.title = "Format root filesystem", .run = &StepFormatRoot},
    {.title = "Format home filesystem", .run = &StepFormatHome},
    {.title = "Mount target filesystems", .run = &StepMount},
    {.title = "Install bootc image", .run = &StepBootcInstall, .weight = 30},
    {.title = "Mount home filesystem", .run = &StepMountHome},
    {.title = "Configure persistent mounts", .run = &StepConfigureMounts},
    {.title = "Configure firstboot identity", .run = &StepFirstboot},
    {.title = "Configure k3s cluster", .run = &StepConfigureK3s},
    {.title = "Stage homed user identity", .run = &StepStageHomedIdentity},
    {.title = "Pre-stage Flatpak refs", .run = &StepFlatpakSeed, .weight = 15},
    {.title = "Pre-stage model catalog", .run = &StepModelsSeed, .weight = 15},
    {.title = "Install systemd-boot", .run = &StepInstallBootloader},
    {.title = "Stage ESP boot files", .run = &StepStageEspBoot},
    {.title = "Sign EFI binaries",
     .run = &StepSign,
     .requires_secure_boot = true},
    {.title = "Enroll Secure Boot keys",
     .run = &StepEnrollKeys,
     .requires_secure_boot = true},
    {.title = "Manage NVRAM boot entry", .run = &StepNvram},
    {.title = "Unmount target filesystems", .run = &StepFinalCleanup},
}};

}  // namespace

std::span<const Step> Steps() { return kSteps; }

}  // namespace puu::engine
