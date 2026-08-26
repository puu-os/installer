// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_CONTEXT_H_
#define PUU_ENGINE_CONTEXT_H_

#include <array>
#include <cstdint>
#include <filesystem>
#include <optional>
#include <string>

#include "base/secret_string.h"
#include "config.h"
#include "engine/image.h"

namespace puu::engine {

// Whether the installer should enrol its own Secure Boot keys. Chosen by the
// user on the Secure Boot screen.
enum class SecureBootMode { kEnroll, kSkip };

// Firmware Secure Boot setup state, as read from efivarfs.
struct SetupMode {
  enum class State {
    // Firmware is in Setup Mode; key enrolment is possible.
    kOn,
    // Firmware has already-enrolled keys; enrolment would fail.
    kOff,
    // Not an EFI system, or the variable could not be read.
    kUnknown,
  };

  State state = State::kUnknown;
  // Only meaningful when `state` is kOn: whether Secure Boot is also enabled.
  std::optional<bool> secure_boot_enabled;
  // Only meaningful when `state` is kUnknown: why detection failed.
  std::string reason;
};

// GPT geometry of the ESP, including its UniquePartitionGUID, needed to write
// an EFI boot entry. `unique_guid` is the mixed-endian on-disk form.
struct EfiPartitionLayout {
  uint32_t partition_number = 0;
  uint64_t start_lba = 0;
  uint64_t size_lba = 0;
  std::array<uint8_t, 16> unique_guid{};
};

// Partition device paths and identifiers discovered during installation.
//
// Ownership note: `efi_uuid` is the ESP's GPT PARTUUID, while `root_uuid` and
// `home_uuid` are filesystem UUIDs. They are not interchangeable; the ESP is
// referenced by PARTUUID because it is formatted FAT, which has no stable UUID.
struct DerivedPartitions {
  std::filesystem::path efi_part;
  std::filesystem::path root_part;
  std::filesystem::path home_part;

  std::string efi_uuid;
  std::string root_uuid;
  std::string home_uuid;

  std::optional<EfiPartitionLayout> layout;
};

// Everything a pipeline run needs. Assembled by the CLI, completed by the UI,
// then cloned into the worker thread.
//
// The worker resets `parts` before running, so partition identifiers always
// come from the current run's format steps.
struct Context {
  Context() = default;
  Context(const Context&) = delete;
  Context& operator=(const Context&) = delete;
  Context(Context&&) = default;
  Context& operator=(Context&&) = default;

  Context Clone() const {
    Context copy;
    copy.config = config;
    copy.source_image = source_image;
    copy.target_imgref = target_imgref;
    copy.image_version = image_version;
    copy.image_manifest = image_manifest;
    copy.drive = drive;
    copy.drive_label = drive_label;
    copy.username = username;
    copy.user_password = user_password.Clone();
    copy.user_shell = user_shell;
    copy.secure_boot = secure_boot;
    copy.k3s_token = k3s_token.Clone();
    copy.k3s_token_file = k3s_token_file;
    copy.parts = parts;
    return copy;
  }

  Config config;

  // Local bootc source, if one was found or supplied. Absent means the image is
  // pulled from the network, which is only supported inside a container.
  std::optional<std::filesystem::path> source_image;
  ImageRef target_imgref;
  std::optional<std::string> image_version;
  ImageManifest image_manifest;

  std::filesystem::path drive;
  std::optional<std::string> drive_label;

  std::optional<Username> username;
  SecretString user_password;
  UserShell user_shell = UserShell::kBash;

  SecureBootMode secure_boot = SecureBootMode::kEnroll;
  SecretString k3s_token;
  std::optional<std::filesystem::path> k3s_token_file;

  DerivedPartitions parts;
};

}  // namespace puu::engine

#endif  // PUU_ENGINE_CONTEXT_H_
