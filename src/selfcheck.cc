// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include <array>
#include <cassert>
#include <cstdint>
#include <cstdio>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <absl/strings/match.h>
#include <spdlog/spdlog.h>

#include "base/status.h"
#include "config.h"
#include "engine/command.h"
#include "engine/context.h"
#include "engine/le_bytes.h"
#include "engine/partition.cc"  // NOLINT(bugprone-suspicious-include)
#include "engine/partition.h"
#include "engine/passwd.cc"  // NOLINT(bugprone-suspicious-include)
#include "engine/passwd.h"
#include "engine/pipeline.h"
#include "engine/secureboot.cc"  // NOLINT(bugprone-suspicious-include)
#include "engine/util.h"

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

// The shipped Puu OS live configuration, verbatim. The installer rejects
// unknown keys, so a schema that drifts from this file aborts the installer
// before the UI ever appears.
constexpr std::string_view kShippedMachineYaml = R"(identity:
  keymap: us
  locale: en_US.UTF-8
  timezone: UTC
  hostname: puu

flatpak:
  manifestPath: /usr/share/puu-os/flatpaks.list
  syncHelper: /usr/libexec/puu-os/flatpak-sync
  liveStore: /var/lib/flatpak

k3s:
  helper: /usr/libexec/puu-os/configure-k3s-cluster

serialConsole:
  x86_64: console=ttyS0,115200
  aarch64: console=ttyAMA0,115200
)";

void CheckShippedConfigSchema() {
  const StatusOr<Config> config = Config::ParseYaml(kShippedMachineYaml);
  assert(config.ok());
  assert(config->identity.hostname.Value() == "puu");
  assert(config->flatpak.manifest_path == "/usr/share/puu-os/flatpaks.list");
  assert(config->flatpak.sync_helper == "/usr/libexec/puu-os/flatpak-sync");
  assert(config->k3s.helper == "/usr/libexec/puu-os/configure-k3s-cluster");
  assert(config->serial_console.at("x86_64").Value() == "console=ttyS0,115200");

  // Defaults must satisfy the schema too, and unknown keys must still fail.
  assert(Config::Defaults().flatpak.manifest_path ==
         "/usr/share/puu-os/flatpaks.list");
  assert(!Config::ParseYaml("flatpak:\n  nosuchkey: x\n").ok());
}

void CheckValidatedString() {
  const StatusOr<Hostname> host1 = Hostname::Parse("host-a");
  assert(host1.ok());
  const StatusOr<Hostname> host2 = Hostname::Parse("host-a");
  assert(host2.ok());
  const StatusOr<Hostname> host3 = Hostname::Parse("host-b");
  assert(host3.ok());
  assert(*host1 == *host2);
  assert(!(*host1 == *host3));
  assert(*host1 != *host3);
}

void CheckMergeFstab() {
  constexpr std::string_view kExisting =
      "# /etc/fstab: static file system information.\n"
      "/dev/root / auto rw 0 1\n"
      "UUID=old-efi /efi vfat defaults 0 2\n"
      "UUID=old-home /var/home ext4 defaults 0 2\n"
      "/dev/sda4 /mnt/data ext4 defaults 0 2\n";
  const std::string merged = MergeFstab(kExisting, "new-efi", "new-home");
  assert(merged.find("/dev/root") == std::string::npos);
  assert(merged.find("old-efi") == std::string::npos);
  assert(merged.find("old-home") == std::string::npos);
  assert(merged.find(
             "PARTUUID=new-efi /efi vfat fmask=0077,dmask=0077,nofail 0 2\n") !=
         std::string::npos);
  assert(merged.find("UUID=new-home /var/home ext4 defaults 0 2\n") !=
         std::string::npos);
  assert(merged.find("/dev/sda4 /mnt/data ext4 defaults 0 2\n") !=
         std::string::npos);
}

void CheckSpawnReportsPreExecFailure() {
  const Status status = Command("/nonexistent/puu-selfcheck-missing").Run();
  assert(!status.ok());
  // The failing step is named, rather than being folded into the child's exit
  // status the way a write to stdout before _exit would be.
  assert(absl::StrContains(status.message(), "execve"));
}

void CheckNewPidNamespace() {
  const Status status = Command("/bin/sh")
                            .Args({"-c", "test \"$$\" -eq 1"})
                            .NewPidNamespace()
                            .Run();
  // Either the namespace was created and the shell saw itself as process 1, or
  // creating it was refused and that specific step is reported.
  assert(status.ok() || absl::StrContains(status.message(), "unshare"));
}

void CheckRunInteractive() {
  const Status ok_status = RunInteractive("/bin/true");
  assert(ok_status.ok());

  const Status fail_status = RunInteractive("/bin/false");
  assert(!fail_status.ok());

  const Status missing_status = RunInteractive("/nonexistent/puu-test-missing");
  assert(!missing_status.ok());
}

void CheckMounts() {
  const auto mounts = Mounts::Read();
  assert(mounts.ok());
  assert(mounts->IsMountPoint("/") || mounts->IsMountPoint("/proc"));
  const auto sources = MountedSources();
  assert(sources.ok());
}

}  // namespace
}  // namespace puu::engine

int main() {
  spdlog::default_logger()->set_level(spdlog::level::off);
  puu::engine::CheckGptLayout();
  puu::engine::CheckAuthenticodeSkippedRanges();
  puu::engine::CheckSha512Crypt();
  puu::engine::CheckShippedConfigSchema();
  puu::engine::CheckValidatedString();
  puu::engine::CheckMergeFstab();
  puu::engine::CheckSpawnReportsPreExecFailure();
  puu::engine::CheckNewPidNamespace();
  puu::engine::CheckRunInteractive();
  puu::engine::CheckMounts();
  std::puts("selfcheck: ok");
  return 0;
}
