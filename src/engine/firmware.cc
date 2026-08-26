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

void ClearImmutable(int fd) {
  int flags = 0;
  if (::ioctl(fd, FS_IOC_GETFLAGS, &flags) != 0) {
    return;
  }
  if ((flags & kFsImmutableFl) == 0) {
    return;
  }
  flags &= ~kFsImmutableFl;
  static_cast<void>(::ioctl(fd, FS_IOC_SETFLAGS, &flags));
}

Status DeleteEfiVariable(const std::filesystem::path& path) {
  if (UniqueFd const fd(::open(path.c_str(), O_WRONLY | O_CLOEXEC));
      fd.Get() >= 0) {
    ClearImmutable(fd.Get());
  }
  std::error_code error;
  std::filesystem::remove(path, error);
  if (error) {
    return InternalError("failed to delete {}: {}", path.string(),
                         error.message());
  }
  return absl::OkStatus();
}

std::optional<std::filesystem::path> FindEfiVariable(
    std::string_view name, const std::filesystem::path& root) {
  std::filesystem::path exact =
      root / fmt::format("{}-{}", name, EfiGuid::Global().ToString());
  std::error_code error;
  if (std::filesystem::exists(exact, error)) {
    return exact;
  }

  const std::string prefix = fmt::format("{}-", name);
  std::optional<std::filesystem::path> best;
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
  out.insert(out.end(), layout.unique_guid.begin(), layout.unique_guid.end());
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
  uint16_t value = 0;
  for (const char c : text) {
    uint16_t digit = 0;
    if (c >= '0' && c <= '9') {
      digit = static_cast<uint16_t>(c - '0');
    } else if (c >= 'A' && c <= 'F') {
      digit = static_cast<uint16_t>(c - 'A' + 10);
    } else {
      return std::nullopt;
    }
    value = static_cast<uint16_t>(value << 4U | digit);
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

Status WriteEfiVariable(const std::filesystem::path& path, uint32_t attributes,
                        std::span<const uint8_t> payload) {
  std::vector<uint8_t> buffer;
  buffer.reserve(4 + payload.size());
  AppendLe<uint32_t>(buffer, attributes);
  buffer.insert(buffer.end(), payload.begin(), payload.end());

  UniqueFd const fd(::open(path.c_str(), O_WRONLY | O_CREAT | O_CLOEXEC, 0644));
  if (fd.Get() < 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("open {}", path.string()));
  }
  ClearImmutable(fd.Get());
  const ssize_t written = ::write(fd.Get(), buffer.data(), buffer.size());
  if (written < 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("write {}", path.string()));
  }
  if (static_cast<size_t>(written) != buffer.size()) {
    return InternalError("short write to {}: {} of {} bytes", path.string(),
                         written, buffer.size());
  }
  return absl::OkStatus();
}

std::filesystem::path EfiVariablePath(std::string_view name,
                                      const EfiGuid& guid) {
  return std::filesystem::path(kEfiVarsDir) /
         fmt::format("{}-{}", name, guid.ToString());
}

EfiGuid EfiGuid::Global() {
  return EfiGuid(std::array<uint8_t, 16>{0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93,
                                         0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0,
                                         0x98, 0x03, 0x2b, 0x8c});
}

EfiGuid EfiGuid::ImageSecurityDatabase() {
  return EfiGuid(std::array<uint8_t, 16>{0xcb, 0xb2, 0x19, 0xd7, 0x3a, 0x3d,
                                         0x96, 0x45, 0xa3, 0xbc, 0xda, 0xd0,
                                         0x0e, 0x67, 0x65, 0x6f});
}

std::string EfiGuid::ToString() const {
  const auto data1 = ReadLe<uint32_t>(bytes_, 0);
  const auto data2 = ReadLe<uint16_t>(bytes_, 4);
  const auto data3 = ReadLe<uint16_t>(bytes_, 6);
  return fmt::format(
      "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
      data1, data2, data3, bytes_[8], bytes_[9], bytes_[10], bytes_[11],
      bytes_[12], bytes_[13], bytes_[14], bytes_[15]);
}

SetupMode DetectSetupMode() {
  std::error_code error;
  if (!std::filesystem::exists(std::filesystem::path(kEfiSysfsDir), error)) {
    return SetupMode{.state = SetupMode::State::kUnknown,
                     .reason = "UEFI firmware variables are unavailable"};
  }
  const std::filesystem::path root{kEfiVarsDir};
  if (!std::filesystem::exists(root, error)) {
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
  std::error_code error;
  if (!std::filesystem::exists(std::filesystem::path(kEfiVarsDir), error)) {
    return FailedPreconditionError("efivarfs is not mounted at {}",
                                   kEfiVarsDir);
  }

  const EfiGuid global = EfiGuid::Global();
  const std::filesystem::path boot_order_path =
      EfiVariablePath("BootOrder", global);
  const bool had_boot_order = std::filesystem::exists(boot_order_path, error);

  std::vector<uint16_t> previous_order;
  if (had_boot_order) {
    PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> raw,
                         ReadFileBytes(boot_order_path));
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

  const std::filesystem::path boot_path =
      EfiVariablePath(fmt::format("Boot{:04X}", *boot_id), global);
  PUU_RETURN_IF_ERROR(WithContext(
      WriteEfiVariable(boot_path, kBootVariableAttributes, load_option),
      fmt::format("failed to write Boot{:04X}", *boot_id)));

  std::vector<uint16_t> new_order{*boot_id};
  for (const uint16_t id : previous_order) {
    if (id != *boot_id) {
      new_order.push_back(id);
    }
  }
  const Status wrote_order = WriteEfiVariable(
      boot_order_path, kBootVariableAttributes, EncodeBootOrder(new_order));
  if (wrote_order.ok()) {
    return absl::OkStatus();
  }

  const Status removed_entry = DeleteEfiVariable(boot_path);
  Status restored_order;
  if (had_boot_order) {
    restored_order = WriteEfiVariable(boot_order_path, kBootVariableAttributes,
                                      EncodeBootOrder(previous_order));
  } else if (std::filesystem::exists(boot_order_path, error)) {
    restored_order = DeleteEfiVariable(boot_order_path);
  }
  return InternalError(
      "failed to update BootOrder: {}; rollback Boot{:04X}: {}; "
      "rollback BootOrder: {}",
      wrote_order.message(), *boot_id, StatusText(removed_entry),
      StatusText(restored_order));
}

}  // namespace puu::engine
