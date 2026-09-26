// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/partition.h"

#include <fcntl.h>
#include <linux/fs.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include <absl/status/status.h>
#include <fmt/format.h>

#include "base/status.h"
#include "config.h"
#include "engine/context.h"
#include "engine/guid.h"
#include "engine/le_bytes.h"
#include "engine/util.h"

namespace puu::engine {

namespace {

constexpr std::string_view kGptSignature = "EFI PART";
constexpr uint32_t kGptRevision = 0x0001'0000;
constexpr uint32_t kHeaderSize = 92;
constexpr uint32_t kEntryCount = 128;
constexpr uint32_t kEntrySize = 128;
constexpr size_t kNameFieldBytes = 72;
constexpr uint64_t kPrimaryHeaderLba = 1;
constexpr uint64_t kPrimaryEntriesLba = 2;

constexpr uint32_t kMbrMaxSectors = 0xFFFF'FFFF;


uint32_t Crc32(std::span<const uint8_t> data);
void EncodePartitionName(std::string_view name, std::span<uint8_t> field);


constexpr size_t kOffCrc = 16;
constexpr size_t kOffMyLba = 24;
constexpr size_t kOffAlternateLba = 32;
constexpr size_t kOffFirstUsable = 40;
constexpr size_t kOffLastUsable = 48;
constexpr size_t kOffDiskGuid = 56;
constexpr size_t kOffEntriesLba = 72;
constexpr size_t kOffEntryCount = 80;
constexpr size_t kOffEntrySize = 84;
constexpr size_t kOffEntriesCrc = 88;

constexpr size_t kOffTypeGuid = 0;
constexpr size_t kOffUniqueGuid = 16;
constexpr size_t kOffFirstLba = 32;
constexpr size_t kOffLastLba = 40;
constexpr size_t kOffAttributes = 48;
constexpr size_t kOffName = 56;

uint64_t DivCeil(uint64_t value, uint64_t divisor) {
  return (value / divisor) + (value % divisor == 0 ? 0 : 1);
}

uint64_t AlignUp(uint64_t value, uint64_t alignment) {
  const uint64_t remainder = value % alignment;
  return remainder == 0 ? value : value + (alignment - remainder);
}

uint64_t AlignDown(uint64_t value, uint64_t alignment) {
  return value - (value % alignment);
}

StatusOr<UniqueFd> OpenDrive(const std::filesystem::path& drive, int flags) {
  UniqueFd fd(::open(drive.c_str(), flags | O_CLOEXEC));
  if (!fd) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("open {}", drive.string()));
  }
  return fd;
}

StatusOr<uint32_t> SectorSize(const UniqueFd& drive,
                              const std::filesystem::path& path) {
  int sector_size = 0;
  if (::ioctl(drive.Get(), BLKSSZGET, &sector_size) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("BLKSSZGET {}", path.string()));
  }
  if (sector_size < 512 || sector_size % 512 != 0) {
    return InternalError("{} reports an unusable sector size of {}",
                         path.string(), sector_size);
  }
  return static_cast<uint32_t>(sector_size);
}

StatusOr<uint64_t> DriveBytes(const UniqueFd& drive,
                              const std::filesystem::path& path) {
  uint64_t bytes = 0;
  if (::ioctl(drive.Get(), BLKGETSIZE64, &bytes) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("BLKGETSIZE64 {}", path.string()));
  }
  return bytes;
}

Status WriteAt(const UniqueFd& drive, std::span<const uint8_t> data,
               uint64_t offset) {
  size_t written = 0;
  while (written < data.size()) {
    const ssize_t count =
        ::pwrite(drive.Get(), data.data() + written, data.size() - written,
                 static_cast<off_t>(offset + written));
    if (count < 0) {
      const int error = errno;
      if (error == EINTR) {
        continue;
      }
      return ErrnoToStatus(error, "write the partition table");
    }
    if (count == 0) {
      return InternalError("short write while writing the partition table");
    }
    written += static_cast<size_t>(count);
  }
  return absl::OkStatus();
}

Status ReadAt(const UniqueFd& drive, std::span<uint8_t> buffer,
              uint64_t offset) {
  size_t read = 0;
  while (read < buffer.size()) {
    const ssize_t count =
        ::pread(drive.Get(), buffer.data() + read, buffer.size() - read,
                static_cast<off_t>(offset + read));
    if (count < 0) {
      const int error = errno;
      if (error == EINTR) {
        continue;
      }
      return ErrnoToStatus(error, "read the partition table");
    }
    if (count == 0) {
      return InternalError("unexpected end of device at offset {}",
                           offset + read);
    }
    read += static_cast<size_t>(count);
  }
  return absl::OkStatus();
}


std::vector<uint8_t> ProtectiveMbr(uint32_t sector_size,
                                   uint64_t disk_sectors) {
  std::vector<uint8_t> block(sector_size, 0);
  const size_t record = 446;
  block[record + 1] = 0x00;
  block[record + 2] = 0x02;
  block[record + 3] = 0x00;
  block[record + 4] = 0xEE;
  block[record + 5] = 0xFF;
  block[record + 6] = 0xFF;
  block[record + 7] = 0xFF;
  PutLe<uint32_t>(block, record + 8, 1);
  const uint64_t sectors = disk_sectors - 1;
  PutLe<uint32_t>(block, record + 12,
                  sectors > kMbrMaxSectors ? kMbrMaxSectors
                                           : static_cast<uint32_t>(sectors));
  block[510] = 0x55;
  block[511] = 0xAA;
  return block;
}

void WriteEntry(std::span<uint8_t> entries, uint32_t number,
                const EfiGuid& type, const EfiGuid& unique, uint64_t first_lba,
                uint64_t last_lba, std::string_view name) {
  const std::span<uint8_t> entry =
      entries.subspan(static_cast<size_t>(number - 1) * kEntrySize, kEntrySize);
  std::ranges::copy(type.Bytes(), entry.begin() + kOffTypeGuid);
  std::ranges::copy(unique.Bytes(), entry.begin() + kOffUniqueGuid);
  PutLe<uint64_t>(entry, kOffFirstLba, first_lba);
  PutLe<uint64_t>(entry, kOffLastLba, last_lba);
  PutLe<uint64_t>(entry, kOffAttributes, 0);
  EncodePartitionName(name, entry.subspan(kOffName, kNameFieldBytes));
}

std::vector<uint8_t> BuildHeader(uint32_t sector_size, uint64_t my_lba,
                                 uint64_t alternate_lba, uint64_t entries_lba,
                                 uint64_t first_usable, uint64_t last_usable,
                                 const EfiGuid& disk_guid, uint32_t entries_crc) {
  std::vector<uint8_t> block(sector_size, 0);
  std::ranges::copy(kGptSignature, block.begin());
  PutLe<uint32_t>(block, 8, kGptRevision);
  PutLe<uint32_t>(block, 12, kHeaderSize);
  PutLe<uint64_t>(block, kOffMyLba, my_lba);
  PutLe<uint64_t>(block, kOffAlternateLba, alternate_lba);
  PutLe<uint64_t>(block, kOffFirstUsable, first_usable);
  PutLe<uint64_t>(block, kOffLastUsable, last_usable);
  std::ranges::copy(disk_guid.Bytes(), block.begin() + kOffDiskGuid);
  PutLe<uint64_t>(block, kOffEntriesLba, entries_lba);
  PutLe<uint32_t>(block, kOffEntryCount, kEntryCount);
  PutLe<uint32_t>(block, kOffEntrySize, kEntrySize);
  PutLe<uint32_t>(block, kOffEntriesCrc, entries_crc);

  PutLe<uint32_t>(block, kOffCrc, Crc32(std::span(block).first(kHeaderSize)));
  return block;
}

uint32_t Crc32(std::span<const uint8_t> data) {
  static const std::array<uint32_t, 256> k_table = [] {
    std::array<uint32_t, 256> table{};
    for (uint32_t index = 0; index < table.size(); ++index) {
      uint32_t value = index;
      for (int bit = 0; bit < 8; ++bit) {
        value = (value & 1) != 0 ? 0xEDB8'8320 ^ (value >> 1) : value >> 1;
      }
      table[index] = value;
    }
    return table;
  }();

  uint32_t crc = 0xFFFF'FFFF;
  for (const uint8_t byte : data) {
    crc = k_table[(crc ^ byte) & 0xFF] ^ (crc >> 8);
  }
  return crc ^ 0xFFFF'FFFF;
}

void EncodePartitionName(std::string_view name, std::span<uint8_t> field) {
  std::ranges::fill(field, uint8_t{0});
  if (field.size() < 4) {
    return;
  }

  const size_t limit = (field.size() / 2) - 1;
  for (size_t i = 0; i < name.size() && i < limit; ++i) {
    const auto character = static_cast<unsigned char>(name[i]);
    field[2 * i] = character < 0x80 ? character : uint8_t{'?'};
  }
}
}  // namespace

uint64_t FirstUsableLba(uint32_t sector_size) {
  return kPrimaryEntriesLba +
         DivCeil(uint64_t{kEntryCount} * kEntrySize, sector_size);
}

uint64_t LastUsableLba(uint64_t disk_sectors, uint32_t sector_size) {
  return disk_sectors - 1 -
         DivCeil(uint64_t{kEntryCount} * kEntrySize, sector_size) - 1;
}

StatusOr<std::array<EfiPartitionLayout, 3>> ComputeGptLayout(
    uint64_t disk_sectors, uint32_t sector_size, uint64_t efi_bytes,
    uint64_t home_bytes) {
  if (sector_size < 512 || sector_size % 512 != 0) {
    return InvalidArgumentError("unusable sector size {}", sector_size);
  }
  if (efi_bytes == 0 || home_bytes == 0) {
    return InvalidArgumentError("partition sizes must be at least one sector");
  }
  const uint64_t entry_sectors =
      DivCeil(uint64_t{kEntryCount} * kEntrySize, sector_size);

  if (disk_sectors < 2 * (entry_sectors + 2) + 3) {
    return FailedPreconditionError("drive of {} sectors is too small for a GPT",
                                   disk_sectors);
  }

  const uint64_t first_usable = FirstUsableLba(sector_size);
  const uint64_t last_usable = LastUsableLba(disk_sectors, sector_size);
  const uint64_t alignment =
      std::max<uint64_t>(1, kAlignmentBytes / sector_size);

  const uint64_t efi_sectors = DivCeil(efi_bytes, sector_size);
  const uint64_t efi_start = AlignUp(first_usable, alignment);
  if (efi_start > last_usable || efi_sectors > last_usable - efi_start + 1) {
    return FailedPreconditionError("no room for the EFI partition");
  }
  const uint64_t efi_end = efi_start + efi_sectors - 1;

  const uint64_t home_sectors = DivCeil(home_bytes, sector_size);
  if (home_sectors > last_usable + 1 - first_usable) {
    return FailedPreconditionError("no room for the home partition");
  }

  const uint64_t home_start =
      AlignDown(last_usable + 1 - home_sectors, alignment);
  const uint64_t root_start = AlignUp(efi_end + 1, alignment);
  if (home_start < first_usable || home_start <= root_start) {
    return FailedPreconditionError("no room for the root partition");
  }
  const uint64_t root_end = home_start - 1;

  return std::array<EfiPartitionLayout, 3>{
      EfiPartitionLayout{.partition_number = kEfiPartitionNumber,
                         .start_lba = efi_start,
                         .size_lba = efi_end - efi_start + 1},
      EfiPartitionLayout{.partition_number = kRootPartitionNumber,
                         .start_lba = root_start,
                         .size_lba = root_end - root_start + 1},
      EfiPartitionLayout{.partition_number = kHomePartitionNumber,
                         .start_lba = home_start,
                         .size_lba = last_usable - home_start + 1},
  };
}

std::filesystem::path PartitionPath(const std::filesystem::path& drive,
                                    uint32_t number) {
  std::string path = drive.string();

  if (!path.empty() &&
      std::isdigit(static_cast<unsigned char>(path.back())) != 0) {
    path.push_back('p');
  }
  return {path + std::to_string(number)};
}

namespace {

Status WipeRange(const UniqueFd& drive, uint64_t offset, uint64_t bytes) {
  static constexpr size_t kChunk = size_t{64} * 1024;
  const std::vector<uint8_t> zeros(kChunk, 0);
  while (bytes > 0) {
    const auto n = static_cast<size_t>(std::min<uint64_t>(kChunk, bytes));
    PUU_RETURN_IF_ERROR(
        WriteAt(drive, std::span<const uint8_t>(zeros).first(n), offset));
    offset += n;
    bytes -= n;
  }
  return absl::OkStatus();
}

}  // namespace

StatusOr<EfiPartitionLayout> PartitionDrive(const std::filesystem::path& drive,
                                            const Config& config) {
  PUU_ASSIGN_OR_RETURN(const UniqueFd file, OpenDrive(drive, O_RDWR | O_EXCL));
  PUU_ASSIGN_OR_RETURN(const uint32_t sector_size, SectorSize(file, drive));
  PUU_ASSIGN_OR_RETURN(const uint64_t bytes, DriveBytes(file, drive));
  const uint64_t disk_sectors = bytes / sector_size;

  PUU_ASSIGN_OR_RETURN(
      const auto layout,
      ComputeGptLayout(disk_sectors, sector_size, config.partitions.efi.Bytes(),
                       config.partitions.home.Bytes()));

  constexpr std::array<std::string_view, 3> kNames = {"EFI", "root", "home"};
  const std::array<EfiGuid, 3> types = {
      EfiGuid::EspType(), EfiGuid::LinuxFilesystemType(),
      EfiGuid::LinuxFilesystemType()};

  EfiPartitionLayout efi = layout[0];
  std::vector<uint8_t> entries(size_t{kEntryCount} * kEntrySize, 0);
  for (size_t i = 0; i < layout.size(); ++i) {
    PUU_ASSIGN_OR_RETURN(const EfiGuid unique, EfiGuid::GenerateRandom());
    WriteEntry(entries, layout[i].partition_number, types[i], unique,
               layout[i].start_lba,
               layout[i].start_lba + layout[i].size_lba - 1, kNames[i]);
    if (layout[i].partition_number == kEfiPartitionNumber) {
      efi.unique_guid = unique;
    }
  }

  PUU_ASSIGN_OR_RETURN(const EfiGuid disk_guid, EfiGuid::GenerateRandom());
  const uint32_t entries_crc = Crc32(entries);
  const uint64_t first_usable = FirstUsableLba(sector_size);
  const uint64_t last_usable = LastUsableLba(disk_sectors, sector_size);
  const uint64_t backup_header_lba = disk_sectors - 1;
  const uint64_t backup_entries_lba = last_usable + 1;

  const std::vector<uint8_t> primary = BuildHeader(
      sector_size, kPrimaryHeaderLba, backup_header_lba, kPrimaryEntriesLba,
      first_usable, last_usable, disk_guid, entries_crc);
  const std::vector<uint8_t> backup = BuildHeader(
      sector_size, backup_header_lba, kPrimaryHeaderLba, backup_entries_lba,
      first_usable, last_usable, disk_guid, entries_crc);
  const std::vector<uint8_t> mbr = ProtectiveMbr(sector_size, disk_sectors);

  constexpr uint64_t kSignatureWipeBytes = uint64_t{1024} * 1024;
  for (const EfiPartitionLayout& part : layout) {
    const uint64_t start_byte = part.start_lba * sector_size;
    const uint64_t size_bytes = part.size_lba * sector_size;
    PUU_RETURN_IF_ERROR(
        WipeRange(file, start_byte, std::min(kSignatureWipeBytes, size_bytes)));
    if (size_bytes > kSignatureWipeBytes) {
      PUU_RETURN_IF_ERROR(
          WipeRange(file, start_byte + size_bytes - kSignatureWipeBytes,
                    kSignatureWipeBytes));
    }
  }

  PUU_RETURN_IF_ERROR(WriteAt(file, mbr, 0));
  PUU_RETURN_IF_ERROR(WriteAt(file, entries, kPrimaryEntriesLba * sector_size));
  PUU_RETURN_IF_ERROR(WriteAt(file, entries, backup_entries_lba * sector_size));
  PUU_RETURN_IF_ERROR(WriteAt(file, primary, kPrimaryHeaderLba * sector_size));
  PUU_RETURN_IF_ERROR(WriteAt(file, backup, backup_header_lba * sector_size));

  if (::fsync(file.Get()) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("fsync {}", drive.string()));
  }
  if (::ioctl(file.Get(), BLKRRPART) != 0) {
    const int error = errno;
    return ErrnoToStatus(
        error, fmt::format("reread the partition table on {}", drive.string()));
  }
  return efi;
}

StatusOr<EfiPartitionLayout> LoadPartitionLayout(
    const std::filesystem::path& drive) {
  PUU_ASSIGN_OR_RETURN(const UniqueFd file, OpenDrive(drive, O_RDONLY));
  PUU_ASSIGN_OR_RETURN(const uint32_t sector_size, SectorSize(file, drive));

  PUU_ASSIGN_OR_RETURN(const uint64_t disk_bytes, DriveBytes(file, drive));
  const uint64_t disk_sectors = disk_bytes / sector_size;

  std::vector<uint8_t> header(sector_size, 0);
  PUU_RETURN_IF_ERROR(ReadAt(file, header, kPrimaryHeaderLba * sector_size));
  if (!std::equal(kGptSignature.begin(), kGptSignature.end(), header.begin())) {
    return NotFoundError("no GPT on {}", drive.string());
  }

  const auto header_size = ReadLe<uint32_t>(header, 12);
  if (header_size < kHeaderSize || header_size > sector_size) {
    return InternalError("GPT on {} has an invalid header size of {}",
                         drive.string(), header_size);
  }
  const auto stored_crc = ReadLe<uint32_t>(header, kOffCrc);
  PutLe<uint32_t>(header, kOffCrc, 0);
  if (Crc32(std::span<const uint8_t>(header).first(header_size)) !=
      stored_crc) {
    return InternalError("GPT header on {} fails its checksum", drive.string());
  }

  const auto entries_lba = ReadLe<uint64_t>(header, kOffEntriesLba);
  const auto entry_count = ReadLe<uint32_t>(header, kOffEntryCount);
  const auto entry_size = ReadLe<uint32_t>(header, kOffEntrySize);
  if (entry_size < kEntrySize || entry_size > 4096 || entry_size % 8 != 0 ||
      entry_count == 0 || entry_count > kEntryCount * 8) {
    return InternalError("GPT on {} has an unusable entry array ({} x {})",
                         drive.string(), entry_count, entry_size);
  }
  if (entries_lba < 2) {
    return InternalError("GPT on {} places its entries at LBA {}",
                         drive.string(), entries_lba);
  }

  if (entries_lba >= disk_sectors) {
    return InternalError(
        "GPT on {} places its entries beyond the disk (LBA {})", drive.string(),
        entries_lba);
  }

  std::vector<uint8_t> entries(size_t{entry_count} * entry_size, 0);
  PUU_RETURN_IF_ERROR(ReadAt(file, entries, entries_lba * sector_size));
  if (Crc32(entries) != ReadLe<uint32_t>(header, kOffEntriesCrc)) {
    return InternalError("GPT entries on {} fail their checksum",
                         drive.string());
  }

  for (uint32_t index = 0; index < entry_count; ++index) {
    const std::span<const uint8_t> entry =
        std::span<const uint8_t>(entries).subspan(
            static_cast<size_t>(index) * entry_size, entry_size);
    std::array<uint8_t, 16> type_bytes{};
    std::ranges::copy(entry.subspan(kOffTypeGuid, 16), type_bytes.begin());
    if (EfiGuid(type_bytes) != EfiGuid::EspType()) {
      continue;
    }
    const auto start = ReadLe<uint64_t>(entry, kOffFirstLba);
    const auto end = ReadLe<uint64_t>(entry, kOffLastLba);
    if (start == 0 || end < start || end >= disk_sectors) {
      return InternalError("EFI partition on {} has an invalid range",
                           drive.string());
    }
    std::array<uint8_t, 16> unique_bytes{};
    std::ranges::copy(entry.subspan(kOffUniqueGuid, 16), unique_bytes.begin());
    return EfiPartitionLayout{.partition_number = index + 1,
                              .start_lba = start,
                              .size_lba = end - start + 1,
                              .unique_guid = EfiGuid(unique_bytes)};
  }
  return NotFoundError("no EFI partition found on {}", drive.string());
}

}  // namespace puu::engine
