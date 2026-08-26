// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_PARTITION_H_
#define PUU_ENGINE_PARTITION_H_

#include <cstdint>
#include <filesystem>
#include <optional>

#include "base/status.h"
#include "config.h"
#include "engine/context.h"

namespace puu::engine {

// Partition numbers written by PartitionDrive. Fixed, because the ESP layout
// and the fstab entries refer to them.
inline constexpr uint32_t kEfiPartitionNumber = 1;
inline constexpr uint32_t kRootPartitionNumber = 2;
inline constexpr uint32_t kHomePartitionNumber = 3;

// Derives a partition device path from a whole-disk path. Disks whose name ends
// in a digit take a "p" separator, so /dev/nvme0n1 yields /dev/nvme0n1p1 while
// /dev/sda yields /dev/sda1.
std::filesystem::path PartitionPath(const std::filesystem::path& drive,
                                    uint32_t number);

// Writes a fresh GPT to `drive`: ESP first, home at the end of the disk, root
// filling the space between.
//
// This destroys all existing data. The drive is opened O_EXCL, so the call
// fails rather than racing another holder. Returns the ESP layout just written,
// including its unique GUID. On return the kernel has been asked to re-read
// the partition table.
StatusOr<EfiPartitionLayout> PartitionDrive(const std::filesystem::path& drive,
                                            const Config& config);

StatusOr<uint64_t> MinTargetSizeBytes(const Config& config);

// Reads the ESP geometry from the partition table on `drive`, for writing an
// EFI boot entry when the layout was not captured during partitioning.
StatusOr<EfiPartitionLayout> LoadPartitionLayout(
    const std::filesystem::path& drive);

}  // namespace puu::engine

#endif  // PUU_ENGINE_PARTITION_H_
