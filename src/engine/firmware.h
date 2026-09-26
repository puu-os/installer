// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_FIRMWARE_H_
#define PUU_ENGINE_FIRMWARE_H_

#include <array>
#include <cstdint>
#include <filesystem>
#include <span>
#include <string>
#include <string_view>

#include "base/status.h"
#include "config.h"
#include "engine/context.h"
#include "engine/guid.h"

namespace puu::engine {

// Reads the firmware Secure Boot setup state from efivarfs.
//
// Never fails: an unreadable or absent variable is reported as
// SetupMode::State::kUnknown with a reason, because a non-EFI system is a
// legitimate configuration rather than an error.
SetupMode DetectSetupMode();

// The efivarfs path of the variable `name` in the `guid` namespace.
std::filesystem::path EfiVariablePath(std::string_view name,
                                      const EfiGuid& guid);

// Writes an EFI variable through efivarfs, clearing FS_IMMUTABLE_FL first.
//
// efivarfs turns exactly one write() into one SetVariable() call, so the
// attribute word and the payload leave in a single buffer: a short or split
// write would set the variable to a truncated value.
Status WriteEfiVariable(const std::filesystem::path& path, uint32_t attributes,
                        std::span<const uint8_t> payload);

struct BootEntry {
  uint16_t id = 0;
  std::string description;
};

// Creates a Boot#### variable for the installed system and puts it first in
// BootOrder.
//
// On failure to update BootOrder the new entry is removed and the previous
// order restored, so a partial change cannot leave the machine unbootable.
Status ManageBootEntry(const EfiPartitionLayout& layout, Arch arch,
                       std::string_view description);

// Requests firmware setup mode on the next reboot by setting
// EFI_OS_INDICATIONS_BOOT_TO_FW_UI in the OsIndications variable.
Status RequestRebootToFirmware();

}  // namespace puu::engine

#endif  // PUU_ENGINE_FIRMWARE_H_
