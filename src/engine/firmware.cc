// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/firmware.h"

#include <fcntl.h>
#include <linux/fs.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <charconv>
#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <optional>
#include <set>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <vector>

#include <absl/status/status.h>
#include <fmt/format.h>
#include <spdlog/spdlog.h>

#include "base/status.h"
#include "config.h"
#include "engine/context.h"
#include "engine/le_bytes.h"
#include "engine/util.h"

namespace puu::engine {
namespace {

constexpr std::string_view kEfiSysfsDir = "/sys/firmware/efi";
constexpr std::string_view kEfiVarsDir = "/sys/firmware/efi/efivars";

constexpr uint32_t kBootVariableAttributes = 0x1 | 0x2 | 0x4;

constexpr uint32_t kLoadOptionActive = 0x1;

constexpr uint8_t kDevicePathTypeMedia = 0x04;
constexpr uint8_t kDevicePathSubTypeHardDrive = 0x01;
constexpr uint8_t kDevicePathSubTypeFilePath = 0x04;
constexpr uint8_t kDevicePathTypeEnd = 0x7f;
constexpr uint8_t kDevicePathSubTypeEndEntire = 0xff;

constexpr uint8_t kHardDriveFormatGpt = 0x02;
constexpr uint8_t kHardDriveSignatureTypeGuid = 0x02;

constexpr uint16_t kHardDriveNodeLength = 42;

constexpr int kFsImmutableFl = 0x0000'0010;

StatusOr<std::vector<uint8_t>> Utf16LeBytes(std::string_view text) {
  static constexpr std::array<uint32_t, 4> kMinCodePoint = {0, 0x80, 0x800,
                                                            0x10000};
  std::vector<uint8_t> out;
  out.reserve(text.size() * 2);
  const auto push = [&out](uint32_t unit) {
    AppendLe<uint16_t>(out, static_cast<uint16_t>(unit));
  };

  size_t index = 0;
  while (index < text.size()) {
    const auto lead = static_cast<uint8_t>(text[index]);
    uint32_t code_point = 0;
    size_t continuations = 0;
    if (lead < 0x80) {
      code_point = lead;
    } else if ((lead & 0xe0U) == 0xc0U) {
      code_point = lead & 0x1fU;
      continuations = 1;
    } else if ((lead & 0xf0U) == 0xe0U) {
      code_point = lead & 0x0fU;
      continuations = 2;
    } else if ((lead & 0xf8U) == 0xf0U) {
      code_point = lead & 0x07U;
      continuations = 3;
    } else {
      return InvalidArgumentError("invalid UTF-8 lead byte {:#04x}", lead);
    }
    if (index + continuations >= text.size()) {
      return InvalidArgumentError("truncated UTF-8 sequence");
    }
    for (size_t i = 1; i <= continuations; ++i) {
      const auto byte = static_cast<uint8_t>(text[index + i]);
      if ((byte & 0xc0U) != 0x80U) {
        return InvalidArgumentError("invalid UTF-8 continuation byte");
      }
      code_point = (code_point << 6U) | (byte & 0x3fU);
    }
    index += continuations + 1;

    if (code_point < kMinCodePoint[continuations] || code_point > 0x10ffff ||
        (code_point >= 0xd800 && code_point <= 0xdfff)) {
      return InvalidArgumentError("invalid UTF-8 code point {:#x}", code_point);
    }
    if (code_point < 0x10000) {
      push(code_point);
    } else {
      const uint32_t offset = code_point - 0x10000;
      push(0xd800 + (offset >> 10U));
      push(0xdc00 + (offset & 0x3ffU));
    }
  }
  return out;
}

void ClearImmutable(const std::filesystem::path& path) {
  if (!Exists(path)) {
    return;
  }
  const UniqueFd fd(::open(path.c_str(), O_RDONLY | O_CLOEXEC));
  if (!fd) {
    return;
  }
  int flags = 0;
  if (::ioctl(fd.Get(), FS_IOC_GETFLAGS, &flags) != 0) {
    return;
  }
  if ((flags & kFsImmutableFl) == 0) {
    return;
  }
  flags &= ~kFsImmutableFl;
  static_cast<void>(::ioctl(fd.Get(), FS_IOC_SETFLAGS, &flags));
}

Status DeleteEfiVariable(const std::filesystem::path& path) {
  ClearImmutable(path);
  return RemoveFile(path);
}

std::optional<std::filesystem::path> FindEfiVariable(
    std::string_view name, const std::filesystem::path& root) {
  std::filesystem::path exact =
      root / fmt::format("{}-{}", name, EfiGuid::Global().ToString());
  if (Exists(exact)) {
    return exact;
  }

  const std::string prefix = fmt::format("{}-", name);
  std::optional<std::filesystem::path> best;
  std::error_code error;
  const std::filesystem::directory_iterator it(root, error);
  if (error) {
    return std::nullopt;
  }
  for (const auto& entry : it) {
    const std::string filename = entry.path().filename().string();
    if (!filename.starts_with(prefix)) {
      continue;
    }
    if (!best.has_value() || entry.path() < *best) {
      best = entry.path();
    }
  }
  return best;
}

std::optional<bool> ReadEfiBool(std::string_view name,
                                const std::filesystem::path& root) {
  const std::optional<std::filesystem::path> path = FindEfiVariable(name, root);
  if (!path.has_value()) {
    return std::nullopt;
  }
  const StatusOr<std::vector<uint8_t>> data = ReadFileBytes(*path);
  if (!data.ok() || data->size() <= 4) {
    return std::nullopt;
  }
  return (*data)[4] != 0;
}

StatusOr<std::vector<uint8_t>> BuildDevicePath(const EfiPartitionLayout& layout,
                                               std::string_view file_path) {
  std::vector<uint8_t> out;
  out.push_back(kDevicePathTypeMedia);
  out.push_back(kDevicePathSubTypeHardDrive);
  AppendLe<uint16_t>(out, kHardDriveNodeLength);
  AppendLe<uint32_t>(out, layout.partition_number);
  AppendLe<uint64_t>(out, layout.start_lba);
  AppendLe<uint64_t>(out, layout.size_lba);
  out.insert(out.end(), layout.unique_guid.Bytes().begin(),
             layout.unique_guid.Bytes().end());
  out.push_back(kHardDriveFormatGpt);
  out.push_back(kHardDriveSignatureTypeGuid);

  PUU_ASSIGN_OR_RETURN(std::vector<uint8_t> path_utf16,
                       Utf16LeBytes(file_path));

  const size_t node_length = 4 + path_utf16.size() + 2;
  if (node_length > 0xffff) {
    return InvalidArgumentError("EFI file path node is too long");
  }
  out.push_back(kDevicePathTypeMedia);
  out.push_back(kDevicePathSubTypeFilePath);
  AppendLe<uint16_t>(out, static_cast<uint16_t>(node_length));
  out.insert(out.end(), path_utf16.begin(), path_utf16.end());
  AppendLe<uint16_t>(out, 0);

  out.push_back(kDevicePathTypeEnd);
  out.push_back(kDevicePathSubTypeEndEntire);
  AppendLe<uint16_t>(out, 4);
  return out;
}

StatusOr<std::vector<uint8_t>> BuildLoadOption(
    std::string_view description, std::span<const uint8_t> device_path) {
  if (device_path.size() > 0xffff) {
    return InvalidArgumentError("EFI device path is too long");
  }
  PUU_ASSIGN_OR_RETURN(std::vector<uint8_t> description_utf16,
                       Utf16LeBytes(description));

  std::vector<uint8_t> out;
  AppendLe<uint32_t>(out, kLoadOptionActive);
  AppendLe<uint16_t>(out, static_cast<uint16_t>(device_path.size()));
  out.insert(out.end(), description_utf16.begin(), description_utf16.end());
  AppendLe<uint16_t>(out, 0);
  out.insert(out.end(), device_path.begin(), device_path.end());
  return out;
}

std::optional<uint16_t> ParseHex4(std::string_view text) {
  if (text.size() != 4) {
    return std::nullopt;
  }
  uint16_t value = 0;
  const auto [stop, error] =
      std::from_chars(text.data(), text.data() + text.size(), value, 16);
  if (error != std::errc() || stop != text.data() + text.size()) {
    return std::nullopt;
  }
  return value;
}

StatusOr<std::set<uint16_t>> ExistingBootIds() {
  const std::string suffix = fmt::format("-{}", EfiGuid::Global().ToString());
  std::set<uint16_t> ids;
  std::error_code error;
  const std::filesystem::directory_iterator it{
      std::filesystem::path(kEfiVarsDir), error};
  if (error) {
    return InternalError("failed to read {}: {}", kEfiVarsDir, error.message());
  }
  for (const auto& entry : it) {
    const std::string name = entry.path().filename().string();
    if (name.size() != 4 + 4 + suffix.size()) {
      continue;
    }
    if (!name.starts_with("Boot") || !name.ends_with(suffix)) {
      continue;
    }
    const std::optional<uint16_t> id = ParseHex4(name.substr(4, 4));
    if (id.has_value()) {
      ids.insert(*id);
    }
  }
  return ids;
}

std::vector<uint16_t> DecodeBootOrder(std::span<const uint8_t> raw) {
  std::vector<uint16_t> order;

  for (size_t offset = 4; offset + 2 <= raw.size(); offset += 2) {
    order.push_back(ReadLe<uint16_t>(raw, offset));
  }
  return order;
}

std::vector<uint8_t> EncodeBootOrder(std::span<const uint16_t> order) {
  std::vector<uint8_t> out;
  out.reserve(order.size() * 2);
  for (const uint16_t id : order) {
    AppendLe<uint16_t>(out, id);
  }
  return out;
}

std::string StatusText(const Status& status) {
  return status.ok() ? "ok" : fmt::format("failed ({})", status.message());
}

}  // namespace

EfiVariable::EfiVariable(std::string_view name, const EfiGuid& guid)
    : path_(std::filesystem::path(kEfiVarsDir) /
            fmt::format("{}-{}", name, guid.ToString())) {}

bool EfiVariable::Exists() const { return puu::engine::Exists(path_); }

StatusOr<std::vector<uint8_t>> EfiVariable::Read() const {
  return ReadFileBytes(path_);
}

Status EfiVariable::Write(uint32_t attributes,
                          std::span<const uint8_t> payload) const {
  std::vector<uint8_t> buffer;
  buffer.reserve(4 + payload.size());
  AppendLe<uint32_t>(buffer, attributes);
  buffer.insert(buffer.end(), payload.begin(), payload.end());

  ClearImmutable(path_);
  UniqueFd const fd(
      ::open(path_.c_str(), O_WRONLY | O_CREAT | O_CLOEXEC, 0644));
  if (!fd) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("open {}", path_.string()));
  }
  const ssize_t written = ::write(fd.Get(), buffer.data(), buffer.size());
  if (written < 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("write {}", path_.string()));
  }
  if (static_cast<size_t>(written) != buffer.size()) {
    return InternalError("short write to {}: {} of {} bytes", path_.string(),
                         written, buffer.size());
  }
  return absl::OkStatus();
}

SetupMode DetectSetupMode() {
  if (!Exists(kEfiSysfsDir)) {
    return SetupMode{.state = SetupMode::State::kUnknown,
                     .reason = "UEFI firmware variables are unavailable"};
  }
  const std::filesystem::path root{kEfiVarsDir};
  if (!Exists(root)) {
    return SetupMode{.state = SetupMode::State::kUnknown,
                     .reason = "efivarfs is not mounted"};
  }

  const std::optional<bool> setup_mode = ReadEfiBool("SetupMode", root);
  if (!setup_mode.has_value()) {
    return SetupMode{.state = SetupMode::State::kUnknown,
                     .reason = "SetupMode EFI variable is missing"};
  }
  if (!*setup_mode) {
    return SetupMode{.state = SetupMode::State::kOff};
  }
  return SetupMode{.state = SetupMode::State::kOn,
                   .secure_boot_enabled = ReadEfiBool("SecureBoot", root)};
}

Status ManageBootEntry(const EfiPartitionLayout& layout, Arch arch,
                       std::string_view description) {
  if (!Exists(kEfiVarsDir)) {
    return FailedPreconditionError("efivarfs is not mounted at {}",
                                   kEfiVarsDir);
  }

  const EfiGuid global = EfiGuid::Global();
  const EfiVariable boot_order("BootOrder", global);
  const bool had_boot_order = boot_order.Exists();

  std::vector<uint16_t> previous_order;
  if (had_boot_order) {
    PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> raw, boot_order.Read());
    previous_order = DecodeBootOrder(raw);
  }

  PUU_ASSIGN_OR_RETURN(const std::set<uint16_t> existing, ExistingBootIds());
  std::optional<uint16_t> boot_id;
  for (uint32_t candidate = 0; candidate <= 0xffff; ++candidate) {
    const auto id = static_cast<uint16_t>(candidate);
    if (std::ranges::find(previous_order, id) != previous_order.end()) {
      continue;
    }
    if (existing.contains(id)) {
      continue;
    }
    boot_id = id;
    break;
  }
  if (!boot_id.has_value()) {
    return FailedPreconditionError("no free Boot#### variable id found");
  }

  const std::string file_path =
      fmt::format(R"(\EFI\systemd\systemd-boot{}.efi)", ArchEfiSuffix(arch));
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> device_path,
                       BuildDevicePath(layout, file_path));
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> load_option,
                       BuildLoadOption(description, device_path));

  const EfiVariable boot_var(fmt::format("Boot{:04X}", *boot_id), global);
  PUU_RETURN_IF_ERROR(
      WithContext(boot_var.Write(kBootVariableAttributes, load_option),
                  fmt::format("failed to write Boot{:04X}", *boot_id)));

  std::vector<uint16_t> new_order{*boot_id};
  for (const uint16_t id : previous_order) {
    if (id != *boot_id) {
      new_order.push_back(id);
    }
  }
  const Status wrote_order =
      boot_order.Write(kBootVariableAttributes, EncodeBootOrder(new_order));
  if (wrote_order.ok()) {
    const EfiVariable boot_next("BootNext", global);
    std::vector<uint8_t> next_payload;
    next_payload.reserve(sizeof(uint16_t));
    AppendLe<uint16_t>(next_payload, *boot_id);
    const Status wrote_next =
        boot_next.Write(kBootVariableAttributes, next_payload);
    if (!wrote_next.ok()) {
      spdlog::warn("failed to set BootNext to {:04X}: {}", *boot_id,
                   wrote_next.message());
    }
    return absl::OkStatus();
  }

  const Status removed_entry = DeleteEfiVariable(boot_var.Path());
  Status restored_order;
  if (had_boot_order) {
    restored_order = boot_order.Write(kBootVariableAttributes,
                                      EncodeBootOrder(previous_order));
  } else if (boot_order.Exists()) {
    restored_order = DeleteEfiVariable(boot_order.Path());
  }
  return InternalError(
      "failed to update BootOrder: {}; rollback Boot{:04X}: {}; "
      "rollback BootOrder: {}",
      wrote_order.message(), *boot_id, StatusText(removed_entry),
      StatusText(restored_order));
}

Status RequestRebootToFirmware() {
  const EfiVariable var("OsIndications", EfiGuid::Global());

  uint64_t indications = 0x0000'0000'0000'0001ULL;
  if (var.Exists()) {
    const StatusOr<std::vector<uint8_t>> raw = var.Read();
    if (raw.ok() && raw->size() >= 4 + sizeof(uint64_t)) {
      indications |= ReadLe<uint64_t>(*raw, 4);
    }
  }

  std::vector<uint8_t> payload;
  AppendLe<uint64_t>(payload, indications);
  return var.Write(kBootVariableAttributes, payload);
}

}  // namespace puu::engine
