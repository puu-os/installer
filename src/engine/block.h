// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_BLOCK_H_
#define PUU_ENGINE_BLOCK_H_

#include <chrono>
#include <cstdint>
#include <filesystem>
#include <optional>
#include <set>
#include <string>
#include <string_view>
#include <vector>

#include "base/status.h"

namespace puu::engine {

enum class BusyReason : std::uint8_t {
  kMounted,
  kCrypt,
  kLvm,
  kRaid,
  kSwap,
  kInUse,
};

std::string_view BusyReasonText(BusyReason reason);

struct Device {
  std::filesystem::path path;
  std::string model;
  uint64_t size = 0;
  // Connection bus, for example "NVME", "USB" or "SATA".
  std::string transport;
  // Absent when the drive is free to be overwritten.
  std::optional<BusyReason> busy_reason;
};

// Lists candidate target drives, newest information first from udev and falling
// back to sysfs. Virtual devices (loop, ram, zram, optical, floppy,
// device-mapper and RAID nodes) are omitted.
StatusOr<std::vector<Device>> ListDisks();

// Mount and swap sources for DiskBusy; read once per scan instead of
// reparsing /proc/self/mountinfo and /proc/swaps per disk.
struct BusySources {
  std::set<std::filesystem::path> mounts;
  std::set<std::filesystem::path> swaps;
};

StatusOr<BusySources> ReadBusySources();

StatusOr<std::optional<BusyReason>> DiskBusy(const std::filesystem::path& drive,
                                             const BusySources& sources);

StatusOr<uint64_t> DriveSizeBytes(const std::filesystem::path& drive);

StatusOr<std::string> CaptureUuid(const std::filesystem::path& partition);

// GPT PARTUUID of `partition`. The ESP is referenced this way because FAT has
// no stable filesystem UUID.
StatusOr<std::string> CapturePartUuid(const std::filesystem::path& partition);

Status WaitForUdevSettle(std::chrono::milliseconds timeout);

// Waits for `path` to appear and be openable. Requires two consecutive
// successful opens, because udev may briefly expose a device under its old name
// while renaming it.
Status WaitForBlockDevice(const std::filesystem::path& path,
                          std::chrono::milliseconds timeout);

}  // namespace puu::engine

#endif  // PUU_ENGINE_BLOCK_H_
