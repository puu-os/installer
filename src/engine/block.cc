// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/block.h"

#include <fcntl.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <cerrno>
#include <charconv>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <limits>
#include <memory>
#include <optional>
#include <set>
#include <sstream>
#include <string>
#include <string_view>
#include <system_error>
#include <type_traits>
#include <vector>

#include <blkid/blkid.h>
#include <fmt/format.h>
#include <libudev.h>

#include "base/status.h"
#include "engine/util.h"

namespace puu::engine {
namespace {

constexpr std::string_view kSysBlock = "/sys/class/block";
constexpr std::string_view kSwapsPath = "/proc/swaps";
constexpr std::string_view kUdevQueuePath = "/run/udev/queue";

constexpr uint64_t kSysfsSectorBytes = 512;

constexpr std::chrono::milliseconds kSettlePollInterval{100};
constexpr std::chrono::milliseconds kDevicePollInterval{500};

constexpr std::array<std::string_view, 7> kVirtualPrefixes = {
    "loop", "ram", "zram", "sr", "fd", "md", "dm-"};

struct UdevDeleter {
  void operator()(udev* context) const { udev_unref(context); }
};
struct UdevEnumerateDeleter {
  void operator()(udev_enumerate* enumerate) const {
    udev_enumerate_unref(enumerate);
  }
};
struct UdevDeviceDeleter {
  void operator()(udev_device* device) const { udev_device_unref(device); }
};
struct BlkidProbeDeleter {
  void operator()(blkid_probe probe) const { blkid_free_probe(probe); }
};

using UdevPtr = std::unique_ptr<udev, UdevDeleter>;
using UdevEnumeratePtr = std::unique_ptr<udev_enumerate, UdevEnumerateDeleter>;
using UdevDevicePtr = std::unique_ptr<udev_device, UdevDeviceDeleter>;
using BlkidProbePtr =
    std::unique_ptr<std::remove_pointer_t<blkid_probe>, BlkidProbeDeleter>;

bool IsVirtualBlock(std::string_view sysname) {
  return std::ranges::any_of(kVirtualPrefixes,
                             [sysname](std::string_view prefix) {
                               return sysname.starts_with(prefix);
                             });
}

std::optional<std::string> SysfsAttr(std::string_view sysname,
                                     std::string_view attribute) {
  const std::filesystem::path path =
      std::filesystem::path(kSysBlock) / sysname / attribute;
  const StatusOr<std::string> line = ReadFirstLine(path);
  if (!line.ok()) {
    return std::nullopt;
  }
  const std::string_view value = Trim(*line);
  if (value.empty()) {
    return std::nullopt;
  }
  return std::string(value);
}

std::optional<uint64_t> SysfsSizeBytes(std::string_view sysname) {
  const std::optional<std::string> value = SysfsAttr(sysname, "size");
  if (!value.has_value()) {
    return std::nullopt;
  }
  uint64_t sectors = 0;
  const char* begin = value->data();
  const char* end = begin + value->size();
  const auto [stop, error] = std::from_chars(begin, end, sectors);
  if (error != std::errc() || stop != end) {
    return std::nullopt;
  }
  if (sectors > std::numeric_limits<uint64_t>::max() / kSysfsSectorBytes) {
    return std::nullopt;
  }
  return sectors * kSysfsSectorBytes;
}

std::optional<std::string> UdevProperty(udev_device* device, const char* key) {
  const char* value = udev_device_get_property_value(device, key);
  if (value == nullptr) {
    return std::nullopt;
  }
  const std::string_view trimmed = Trim(value);
  if (trimmed.empty()) {
    return std::nullopt;
  }
  return std::string(trimmed);
}

std::string DiskModel(udev_device* device, std::string_view sysname) {
  if (std::optional<std::string> model = UdevProperty(device, "ID_MODEL")) {
    std::replace(model->begin(), model->end(), '_', ' ');
    return *model;
  }
  return SysfsAttr(sysname, "device/model").value_or(std::string());
}

// Prefers the WWN, which is globally unique and stable across reboots, and
// falls back to the serial that udev synthesises for buses without one.
std::string DiskIdentity(udev_device* device) {
  for (const char* key : {"ID_WWN_WITH_EXTENSION", "ID_WWN", "ID_SERIAL"}) {
    if (std::optional<std::string> value = UdevProperty(device, key)) {
      return *value;
    }
  }
  return {};
}

std::string DiskTransport(udev_device* device, std::string_view sysname) {
  if (std::optional<std::string> bus = UdevProperty(device, "ID_BUS")) {
    std::transform(bus->begin(), bus->end(), bus->begin(), [](char character) {
      return static_cast<char>(
          std::toupper(static_cast<unsigned char>(character)));
    });
    return *bus;
  }

  if (sysname.starts_with("nvme")) {
    return "NVME";
  }
  return {};
}

StatusOr<std::vector<std::string>> PartitionSysnames(std::string_view disk) {
  std::vector<std::string> partitions;
  std::error_code error;
  const std::filesystem::path root = std::filesystem::path("/sys/block") / disk;
  std::filesystem::directory_iterator entry(root, error);
  if (error) {
    return FilesystemError(error, fmt::format("list {}", root.string()));
  }
  const std::filesystem::directory_iterator end;
  while (entry != end) {
    const std::string name = entry->path().filename().string();
    if (std::string_view(name).starts_with(disk)) {
      std::error_code marker_error;
      const bool partition = std::filesystem::is_regular_file(
          entry->path() / "partition", marker_error);
      if (marker_error &&
          marker_error != std::errc::no_such_file_or_directory) {
        return FilesystemError(
            marker_error, fmt::format("inspect {}", entry->path().string()));
      }
      if (partition) {
        partitions.push_back(name);
      }
    }
    entry.increment(error);
    if (error) {
      return FilesystemError(error, fmt::format("list {}", root.string()));
    }
  }
  return partitions;
}

StatusOr<std::set<std::filesystem::path>> SwapDevices() {
  if (!Exists(kSwapsPath)) {
    return std::set<std::filesystem::path>();
  }
  PUU_ASSIGN_OR_RETURN(const std::string text, ReadFile(kSwapsPath));
  std::set<std::filesystem::path> devices;
  std::istringstream file(text);
  std::string line;
  std::getline(file, line);
  while (std::getline(file, line)) {
    const std::string_view source =
        std::string_view(line).substr(0, line.find_first_of(" \t"));
    if (source.starts_with("/dev/")) {
      devices.insert(Canonical(std::filesystem::path(source)));
    }
  }
  if (file.bad()) {
    return InternalError("failed to parse {}", kSwapsPath);
  }
  return devices;
}

BusyReason HolderReason(std::string_view holder) {
  const std::string uuid = SysfsAttr(holder, "dm/uuid").value_or(std::string());
  if (uuid.starts_with("CRYPT-")) {
    return BusyReason::kCrypt;
  }
  if (uuid.starts_with("LVM-")) {
    return BusyReason::kLvm;
  }
  if (holder.starts_with("md") || uuid.find("raid") != std::string::npos) {
    return BusyReason::kRaid;
  }
  return BusyReason::kInUse;
}

StatusOr<std::optional<std::string>> FirstHolder(std::string_view sysname) {
  std::error_code error;
  const std::filesystem::path holders =
      std::filesystem::path(kSysBlock) / sysname / "holders";
  const std::filesystem::directory_iterator entry(holders, error);
  if (error == std::errc::no_such_file_or_directory) {
    return std::nullopt;
  }
  if (error) {
    return FilesystemError(error, fmt::format("list {}", holders.string()));
  }
  if (entry == std::filesystem::directory_iterator()) {
    return std::nullopt;
  }
  return entry->path().filename().string();
}

StatusOr<std::string> ProbeValue(const std::filesystem::path& device,
                                 const char* field, bool partitions) {
  const BlkidProbePtr probe(blkid_new_probe_from_filename(device.c_str()));
  if (probe == nullptr) {
    const int error = errno;
    return ErrnoToStatus(error == 0 ? EIO : error,
                         fmt::format("probe {}", device.string()));
  }
  if (partitions) {
    blkid_probe_enable_partitions(probe.get(), 1);
    blkid_probe_set_partitions_flags(probe.get(), BLKID_PARTS_ENTRY_DETAILS);
  }
  if (blkid_do_safeprobe(probe.get()) < 0) {
    return InternalError("failed to probe {}", device.string());
  }
  const char* value = nullptr;
  size_t length = 0;
  if (blkid_probe_lookup_value(probe.get(), field, &value, &length) != 0 ||
      value == nullptr || length == 0) {
    return NotFoundError("unable to determine {} for {}", field,
                         device.string());
  }

  return std::string(value, length - 1);
}

bool OpensSuccessfully(const std::filesystem::path& path) {
  const UniqueFd fd(::open(path.c_str(), O_RDONLY | O_CLOEXEC));
  return static_cast<bool>(fd);
}

}  // namespace

std::string_view BusyReasonText(BusyReason reason) {
  switch (reason) {
    case BusyReason::kMounted:
      return "mounted";
    case BusyReason::kCrypt:
      return "encrypted";
    case BusyReason::kLvm:
      return "LVM member";
    case BusyReason::kRaid:
      return "RAID member";
    case BusyReason::kSwap:
      return "swap";
    case BusyReason::kInUse:
      return "in use";
  }
  return "in use";
}

StatusOr<std::vector<Device>> ListDisks() {
  const UdevPtr context(udev_new());
  if (context == nullptr) {
    return InternalError("failed to create a udev context");
  }
  const UdevEnumeratePtr enumerate(udev_enumerate_new(context.get()));
  if (enumerate == nullptr) {
    return InternalError("failed to create a udev enumeration");
  }
  if (const int rc =
          udev_enumerate_add_match_subsystem(enumerate.get(), "block");
      rc < 0) {
    return ErrnoToStatus(-rc, "match the block subsystem");
  }
  if (const int rc = udev_enumerate_scan_devices(enumerate.get()); rc < 0) {
    return ErrnoToStatus(-rc, "enumerate block devices");
  }

  std::vector<Device> disks;

  const StatusOr<BusySources> busy_sources = ReadBusySources();
  udev_list_entry* entry = nullptr;
  udev_list_entry_foreach(entry,
                          udev_enumerate_get_list_entry(enumerate.get())) {
    const char* syspath = udev_list_entry_get_name(entry);
    if (syspath == nullptr) {
      continue;
    }
    const UdevDevicePtr device(
        udev_device_new_from_syspath(context.get(), syspath));
    if (device == nullptr) {
      continue;
    }

    const char* devtype = udev_device_get_devtype(device.get());
    if (devtype == nullptr || std::string_view(devtype) != "disk") {
      continue;
    }
    const char* sysname = udev_device_get_sysname(device.get());
    if (sysname == nullptr || IsVirtualBlock(sysname)) {
      continue;
    }

    const char* devnode = udev_device_get_devnode(device.get());
    const std::filesystem::path path =
        devnode != nullptr ? std::filesystem::path(devnode)
                           : std::filesystem::path("/dev") / sysname;

    std::optional<BusyReason> busy;
    if (busy_sources.ok()) {
      PUU_ASSIGN_OR_RETURN(busy, DiskBusy(path, *busy_sources));
    } else {
      busy = BusyReason::kInUse;
    }
    disks.push_back(Device{
        .path = path,
        .id = DiskIdentity(device.get()),
        .model = DiskModel(device.get(), sysname),

        .size = SysfsSizeBytes(sysname).value_or(0),
        .transport = DiskTransport(device.get(), sysname),
        .busy_reason = busy,
    });
  }

  std::ranges::sort(disks, [](const Device& left, const Device& right) {
    return left.path < right.path;
  });
  return disks;
}

StatusOr<BusySources> ReadBusySources() {
  BusySources sources;
  PUU_ASSIGN_OR_RETURN(sources.mounts, MountedSources());
  PUU_ASSIGN_OR_RETURN(sources.swaps, SwapDevices());
  return sources;
}

StatusOr<std::optional<BusyReason>> DiskBusy(const std::filesystem::path& drive,
                                             const BusySources& sources) {
  const std::string disk = drive.filename().string();
  if (disk.empty()) {
    return InvalidArgumentError("invalid drive path '{}'", drive.string());
  }

  std::vector<std::string> nodes = {disk};
  PUU_ASSIGN_OR_RETURN(const std::vector<std::string> partitions,
                       PartitionSysnames(disk));
  nodes.insert(nodes.end(), partitions.begin(), partitions.end());

  for (const std::string& node : nodes) {
    const std::filesystem::path devnode = std::filesystem::path("/dev") / node;
    const std::filesystem::path canonical = Canonical(devnode);
    if (sources.mounts.contains(canonical)) {
      return BusyReason::kMounted;
    }
    if (sources.swaps.contains(canonical)) {
      return BusyReason::kSwap;
    }
    PUU_ASSIGN_OR_RETURN(const std::optional<std::string> holder,
                         FirstHolder(node));
    if (holder.has_value()) {
      return HolderReason(*holder);
    }
    const UniqueFd fd(::open(devnode.c_str(), O_RDONLY | O_EXCL | O_CLOEXEC));
    if (fd) {
      continue;
    }
    const int error = errno;
    if (error == EBUSY) {
      return BusyReason::kInUse;
    }
    return ErrnoToStatus(error,
                         fmt::format("open {} exclusively", devnode.string()));
  }
  return std::nullopt;
}

StatusOr<std::string> DriveIdentity(const std::filesystem::path& drive) {
  const UdevPtr context(udev_new());
  if (context == nullptr) {
    return InternalError("failed to create a udev context");
  }
  const std::string name = drive.filename().string();
  if (name.empty()) {
    return InvalidArgumentError("invalid drive path '{}'", drive.string());
  }
  const UdevDevicePtr device(udev_device_new_from_subsystem_sysname(
      context.get(), "block", name.c_str()));
  if (device == nullptr) {
    return NotFoundError("no udev record for '{}'", drive.string());
  }
  return DiskIdentity(device.get());
}

StatusOr<uint64_t> DriveSizeBytes(const std::filesystem::path& drive) {
  const std::string name = drive.filename().string();
  if (name.empty()) {
    return InvalidArgumentError("invalid drive path '{}'", drive.string());
  }
  const std::optional<uint64_t> size = SysfsSizeBytes(name);
  if (!size.has_value()) {
    return NotFoundError("unable to determine the size of '{}'",
                         drive.string());
  }
  return *size;
}

StatusOr<std::string> CaptureUuid(const std::filesystem::path& partition) {
  return ProbeValue(partition, "UUID", /*partitions=*/false);
}

StatusOr<std::string> CapturePartUuid(const std::filesystem::path& partition) {
  return ProbeValue(partition, "PART_ENTRY_UUID", /*partitions=*/true);
}
StatusOr<std::string> ProbeFilesystemType(const std::filesystem::path& device) {
  return ProbeValue(device, "TYPE", /*partitions=*/false);
}

StatusOr<std::optional<std::filesystem::path>> FindDeviceByLabel(
    std::string_view label) {
  const std::string wanted(label);
  char* device = blkid_evaluate_tag("LABEL", wanted.c_str(), nullptr);
  if (device == nullptr) {
    return std::nullopt;
  }
  const std::unique_ptr<char, decltype(&std::free)> owned(device, &std::free);
  return std::filesystem::path(owned.get());
}

Status WaitForUdevSettle(std::chrono::milliseconds timeout) {
  const Status status = WaitUntil([] { return !Exists(kUdevQueuePath); },
                                  timeout, kSettlePollInterval);
  return WithContext(status, "waiting for udev to settle");
}

Status WaitForBlockDevice(const std::filesystem::path& path,
                          std::chrono::milliseconds timeout) {
  int consecutive = 0;
  const Status status = WaitUntil(
      [&path, &consecutive] {
        consecutive = OpensSuccessfully(path) ? consecutive + 1 : 0;
        return consecutive >= 2;
      },
      timeout, kDevicePollInterval);
  return WithContext(status, fmt::format("waiting for {}", path.string()));
}

}  // namespace puu::engine
