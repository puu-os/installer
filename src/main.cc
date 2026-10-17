// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include <sys/reboot.h>

#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <optional>
#include <string>
#include <utility>

#include <spdlog/spdlog.h>

#include "base/secret_string.h"
#include "base/status.h"
#include "cli.h"
#include "config.h"
#include "engine/command.h"
#include "engine/context.h"
#include "engine/firmware.h"
#include "engine/image.h"
#include "engine/pipeline.h"
#include "engine/util.h"
#include "tui/app.h"
#include "tui/logging.h"

namespace puu {
namespace {

StatusOr<std::filesystem::path> ResolveConfigPath(const CommandLine& options) {
  if (options.config_path.has_value()) {
    return *options.config_path;
  }

  const char* from_env = std::getenv("PUU_INSTALLER_CONFIG");
  if (from_env != nullptr && *from_env != '\0') {
    return std::filesystem::path(from_env);
  }

  return InvalidArgumentError(
      "no configuration file: pass --config-path or set "
      "PUU_INSTALLER_CONFIG");
}

StatusOr<engine::Context> BuildContext(const CommandLine& options) {
  PUU_ASSIGN_OR_RETURN(const std::filesystem::path config_path,
                       ResolveConfigPath(options));
  PUU_ASSIGN_OR_RETURN(Config config, Config::Load(config_path));

  if (options.k3s_role.has_value()) {
    config.k3s.role = *options.k3s_role;
  }

  std::optional<std::filesystem::path> source_image = options.source_image;
  if (!source_image.has_value()) {
    PUU_ASSIGN_OR_RETURN(source_image, engine::DefaultSourceImage(config));
  }

  if (!source_image.has_value() && !engine::RunningInContainer()) {
    return FailedPreconditionError(
        "no local bootc source image found; expected {}/image.tar "
        "or --source-image",
        config.image.source_mount.string());
  }

  SecretString k3s_token;
  if (options.k3s_token_file.has_value()) {
    PUU_ASSIGN_OR_RETURN(k3s_token,
                         engine::ReadFirstSecretLine(*options.k3s_token_file));
  }

  if (config.k3s.role == K3sRole::kAgent &&
      (!config.k3s.server_url.has_value() || k3s_token.Empty())) {
    return InvalidArgumentError(
        "k3s agent role requires k3s.serverUrl and --k3s-token-file");
  }

  engine::ImageManifest image_manifest;
  if (source_image.has_value()) {
    PUU_ASSIGN_OR_RETURN(image_manifest,
                         engine::LoadImageManifest(*source_image));
  }
  PUU_ASSIGN_OR_RETURN(ImageRef target_imgref,
                       engine::LoadTargetImgRef(image_manifest, config));

  engine::Context context;
  context.config = std::move(config);
  context.source_image = source_image;
  context.target_imgref = std::move(target_imgref);
  context.image_version = image_manifest.version;
  context.image_manifest = std::move(image_manifest);
  context.k3s_token = std::move(k3s_token);
  context.k3s_token_file = options.k3s_token_file;
  return context;
}

int RunInstaller(engine::Context context) {
  tui::Installer installer(std::move(context));

  for (;;) {
    switch (installer.Run()) {
      case tui::Trap::kQuit:
        return EXIT_SUCCESS;

      case tui::Trap::kReboot: {
        engine::SyncAll();
        ::reboot(RB_AUTOBOOT);

        std::fprintf(stderr, "puu-installer: reboot failed\n");
        return EXIT_FAILURE;
      }

      case tui::Trap::kRebootFirmware: {
        const Status status = engine::RequestRebootToFirmware();
        if (!status.ok()) {
          spdlog::warn("failed to set reboot to firmware setup: {}",
                       status.message());
        }
        engine::SyncAll();
        ::reboot(RB_AUTOBOOT);

        std::fprintf(stderr, "puu-installer: reboot failed\n");
        return EXIT_FAILURE;
      }

      case tui::Trap::kShell: {
        const std::filesystem::path shell = engine::DefaultShell();
        const Status status = engine::Command::RunInteractive(shell);
        if (!status.ok()) {
          spdlog::warn("recovery shell exited: {}", status.message());
        }

        installer = tui::Installer(std::move(installer).IntoContext());
        break;
      }
    }
  }
}

int Main(int argc, char** argv) {
  const ParseResult parsed = ParseCommandLine(argc, argv);
  switch (parsed.outcome) {
    case ParseOutcome::kExitSuccess:
      return EXIT_SUCCESS;
    case ParseOutcome::kExitFailure:
      return EXIT_FAILURE;
    case ParseOutcome::kRun:
      break;
  }

  if (!engine::RunningAsRoot()) {
    std::fprintf(stderr, "puu-installer: must be run as root\n");
    return EXIT_FAILURE;
  }

  if (const Status status = tui::Init(); !status.ok()) {
    std::fprintf(stderr, "puu-installer: %s\n",
                 std::string(status.message()).c_str());
    return EXIT_FAILURE;
  }
  spdlog::info("installer started");

  if (const Status status = engine::EnterPrivateMountNamespace();
      !status.ok()) {
    std::fprintf(stderr, "puu-installer: %s\n",
                 std::string(status.message()).c_str());
    return EXIT_FAILURE;
  }

  StatusOr<engine::Context> context = BuildContext(parsed.options);
  if (!context.ok()) {
    std::fprintf(stderr, "puu-installer: %s\n",
                 std::string(context.status().message()).c_str());
    return EXIT_FAILURE;
  }

  return RunInstaller(*std::move(context));
}

}  // namespace
}  // namespace puu

int main(int argc, char** argv) { return puu::Main(argc, argv); }
