// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include <array>
#include <cassert>
#include <cstdint>
#include <cstdio>
#include <string>
#include <utility>
#include <vector>

#include "base/status.h"
#include "engine/context.h"
#include "engine/le_bytes.h"
#include "engine/partition.cc"
#include "engine/partition.h"
#include "engine/passwd.cc"
#include "engine/passwd.h"
#include "engine/secureboot.cc"

namespace puu::engine {
namespace {

constexpr uint64_t kSectorSize = 512;
constexpr uint64_t kDiskSectors = (uint64_t{512} << 30) / kSectorSize;
constexpr uint64_t kEfiBytes = uint64_t{1} << 30;
constexpr uint64_t kHomeBytes = uint64_t{128} << 30;

void CheckGptLayout() {
  const StatusOr<std::array<EfiPartitionLayout, 3>> layout =
      ComputeGptLayout(kDiskSectors, kSectorSize, kEfiBytes, kHomeBytes);
  assert(layout.ok());

  const EfiPartitionLayout& efi = (*layout)[0];
  const EfiPartitionLayout& root = (*layout)[1];
  const EfiPartitionLayout& home = (*layout)[2];

  assert(efi.partition_number == kEfiPartitionNumber);
  assert(root.partition_number == kRootPartitionNumber);
  assert(home.partition_number == kHomePartitionNumber);

  const uint64_t first_usable = FirstUsableLba(kSectorSize);
  const uint64_t last_usable = LastUsableLba(kDiskSectors, kSectorSize);
  const uint64_t alignment = kAlignmentBytes / kSectorSize;

  for (const EfiPartitionLayout& part : *layout) {
    assert(part.size_lba > 0);
    assert(part.start_lba >= first_usable);
    assert(part.start_lba + part.size_lba - 1 <= last_usable);
    assert(part.start_lba % alignment == 0);
  }

  assert(efi.start_lba + efi.size_lba <= root.start_lba);
  assert(root.start_lba + root.size_lba == home.start_lba);
  assert(home.start_lba + home.size_lba - 1 == last_usable);

  assert(efi.size_lba * kSectorSize >= kEfiBytes);
  assert(home.size_lba * kSectorSize >= kHomeBytes);

  assert(!ComputeGptLayout(2048, kSectorSize, kEfiBytes, kHomeBytes).ok());
  assert(!ComputeGptLayout(kDiskSectors, 500, kEfiBytes, kHomeBytes).ok());
  assert(!ComputeGptLayout(kDiskSectors, kSectorSize, 0, kHomeBytes).ok());
}

void CheckAuthenticodeSkippedRanges() {
  constexpr size_t kElfanew = 0x80;
  constexpr size_t kOptionalHeader = kElfanew + 24;
  std::vector<uint8_t> image(1024, 0);
  image[0] = 'M';
  image[1] = 'Z';
  PutLe<uint32_t>(image, 0x3c, static_cast<uint32_t>(kElfanew));
  image[kElfanew] = 'P';
  image[kElfanew + 1] = 'E';
  PutLe<uint16_t>(image, kElfanew + 6, 1);
  PutLe<uint16_t>(image, kElfanew + 20, 240);
  PutLe<uint16_t>(image, kOptionalHeader, 0x20b);
  PutLe<uint32_t>(image, kOptionalHeader + 60, 512);
  PutLe<uint32_t>(image, kOptionalHeader + 108, 16);

  const StatusOr<std::vector<std::pair<size_t, size_t>>> ranges =
      AuthenticodeSkippedRanges(image);
  assert(ranges.ok());

  assert(ranges->size() == 2);
  for (const std::pair<size_t, size_t>& range : *ranges) {
    assert(range.first < range.second);
    assert(range.second <= image.size());
  }
  assert((*ranges)[0].second - (*ranges)[0].first == 4);
  assert((*ranges)[1].second - (*ranges)[1].first == 8);
  assert((*ranges)[0].second <= (*ranges)[1].first);

  assert(!AuthenticodeSkippedRanges(std::span(image).first(2)).ok());
}

void CheckSha512Crypt() {
  static_assert(kSha512CryptRounds == 5000);

  const StatusOr<std::string> hashed =
      Sha512Crypt("Hello world!", "saltstring");
  assert(hashed.ok());
  assert(*hashed ==
         "svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdc"
         "bYEdFCoEOfaS35inz1");

  const StatusOr<std::string> empty = Sha512Crypt("", "");
  assert(empty.ok());
  assert(empty->size() == 86);

  const StatusOr<std::string> other = Sha512Crypt("Hello world!", "saltstrinh");
  assert(other.ok());
  assert(*other != *hashed);
}

}  // namespace
}  // namespace puu::engine

int main() {
  puu::engine::CheckGptLayout();
  puu::engine::CheckAuthenticodeSkippedRanges();
  puu::engine::CheckSha512Crypt();
  std::puts("selfcheck: ok");
  return 0;
}
