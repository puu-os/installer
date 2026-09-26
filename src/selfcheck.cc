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

#include <unistd.h>

#include <absl/strings/match.h>
#include <spdlog/spdlog.h>

#include "base/secret_string.h"
#include "base/status.h"
#include "config.h"
#include "engine/block.h"
#include "engine/command.h"
#include "engine/context.h"
#include "engine/firmware.h"
#include "engine/guid.h"
#include "engine/le_bytes.h"
#include "engine/partition.h"
#include "engine/passwd.h"
#include "engine/pipeline.h"
#include "engine/secureboot.h"
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
  manifestPath: /usr/share/puu/flatpaks.list
  syncHelper: /usr/libexec/puu/flatpak-sync
  liveStore: /var/lib/flatpak

k3s:
  helper: /usr/libexec/puu/configure-k3s-cluster

serialConsole:
  x86_64: console=ttyS0,115200
  aarch64: console=ttyAMA0,115200
)";

void CheckShippedConfigSchema() {
  const StatusOr<Config> config = Config::ParseYaml(kShippedMachineYaml);
  assert(config.ok());
  assert(config->identity.hostname.Value() == "puu");
  assert(config->flatpak.manifest_path == "/usr/share/puu/flatpaks.list");
  assert(config->flatpak.sync_helper == "/usr/libexec/puu/flatpak-sync");
  assert(config->k3s.helper == "/usr/libexec/puu/configure-k3s-cluster");
  assert(config->serial_console.at("x86_64").Value() == "console=ttyS0,115200");

  // Defaults must satisfy the schema too, and unknown keys must still fail.
  assert(Config::Defaults().flatpak.manifest_path ==
         "/usr/share/puu/flatpaks.list");
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

void CheckFileUtils() {
  const std::filesystem::path path =
      std::filesystem::temp_directory_path() / "puu-selfcheck-fileutils.tmp";
  RemoveFile(path).IgnoreError();

  assert(!Exists(path));
  assert(!IsRegularFile(path));

  const Status write_status = WriteFile(path, "hello world\n", 0600);
  assert(write_status.ok());
  assert(Exists(path));
  assert(IsRegularFile(path));

  const StatusOr<std::string> read_content = ReadFile(path);
  assert(read_content.ok());
  assert(*read_content == "hello world\n");

  const Status replace_status = ReplaceFile(path, "replaced\n");
  assert(replace_status.ok());

  const StatusOr<std::string> replaced_content = ReadFile(path);
  assert(replaced_content.ok());
  assert(*replaced_content == "replaced\n");

  const std::filesystem::path copy_path =
      std::filesystem::temp_directory_path() / "puu-selfcheck-copy.tmp";
  RemoveFile(copy_path).IgnoreError();
  assert(CopyFile(path, copy_path).ok());
  assert(Exists(copy_path));
  assert(IsRegularFile(copy_path));
  const StatusOr<std::string> copy_content = ReadFile(copy_path);
  assert(copy_content.ok() && *copy_content == "replaced\n");

  const std::filesystem::path symlink_path =
      std::filesystem::temp_directory_path() / "puu-selfcheck-symlink.tmp";
  RemoveFile(symlink_path).IgnoreError();
  assert(CreateSymlink(copy_path, symlink_path).ok());
  assert(Exists(symlink_path));
  const StatusOr<std::filesystem::path> target = ReadSymlink(symlink_path);
  assert(target.ok() && *target == copy_path);

  const std::filesystem::path dir_path =
      std::filesystem::temp_directory_path() / "puu-selfcheck-tree.tmp";
  RemoveTree(dir_path).IgnoreError();
  assert(CreateDirectories(dir_path / "subdir").ok());
  assert(WriteFile(dir_path / "subdir" / "leaf.txt", "data", 0600).ok());
  assert(Exists(dir_path / "subdir" / "leaf.txt"));
  assert(RemoveTree(dir_path).ok());
  assert(!Exists(dir_path));

  assert(RemoveFile(symlink_path).ok());
  assert(!Exists(symlink_path));
  const std::filesystem::path renamed_path =
      std::filesystem::temp_directory_path() / "puu-selfcheck-renamed.tmp";
  RemoveFile(renamed_path).IgnoreError();
  assert(Rename(copy_path, renamed_path).ok());
  assert(!Exists(copy_path));
  assert(Exists(renamed_path));
  assert(RemoveFile(renamed_path).ok());
  assert(!Exists(renamed_path));
  assert(RemoveFile(path).ok());
  assert(!Exists(path));
}

void CheckCreatePipe() {
  const StatusOr<Pipe> pipe = CreatePipe();
  assert(pipe.ok());
  assert(pipe->read);
  assert(pipe->write);

  constexpr std::string_view kData = "pipe selfcheck test";
  const ssize_t written =
      ::write(pipe->write.Get(), kData.data(), kData.size());
  assert(written == static_cast<ssize_t>(kData.size()));

  std::array<char, 32> buffer = {};
  const ssize_t read_bytes =
      ::read(pipe->read.Get(), buffer.data(), buffer.size());
  assert(read_bytes == static_cast<ssize_t>(kData.size()));
  assert(std::string_view(buffer.data(), static_cast<size_t>(read_bytes)) ==
         kData);
}

void CheckBlockProbing() {
  const StatusOr<std::optional<std::filesystem::path>> missing_device =
      FindDeviceByLabel("puu-nonexistent-label-test");
  assert(missing_device.ok());
  assert(!missing_device->has_value());

  const StatusOr<std::string> missing_type =
      ProbeFilesystemType("/nonexistent/puu-test-block");
  assert(!missing_type.ok());
}

void CheckEfiGuid() {
  const EfiGuid global = EfiGuid::Global();
  assert(global.ToString() == "8be4df61-93ca-11d2-aa0d-00e098032b8c");
  const StatusOr<EfiGuid> parsed_global =
      EfiGuid::Parse("8be4df61-93ca-11d2-aa0d-00e098032b8c");
  assert(parsed_global.ok());
  assert(*parsed_global == global);

  const EfiGuid sec_db = EfiGuid::ImageSecurityDatabase();
  assert(sec_db.ToString() == "d719b2cb-3d3a-4596-a3bc-dad00e67656f");
  const StatusOr<EfiGuid> parsed_sec_db =
      EfiGuid::Parse("d719b2cb-3d3a-4596-a3bc-dad00e67656f");
  assert(parsed_sec_db.ok());
  assert(*parsed_sec_db == sec_db);

  const EfiGuid esp = EfiGuid::EspType();
  assert(esp.ToString() == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
  assert(*EfiGuid::Parse("c12a7328-f81f-11d2-ba4b-00a0c93ec93b") == esp);

  const EfiGuid linux_fs = EfiGuid::LinuxFilesystemType();
  assert(linux_fs.ToString() == "0fc63daf-8483-4772-8e79-3d69d8477de4");
  assert(*EfiGuid::Parse("0fc63daf-8483-4772-8e79-3d69d8477de4") == linux_fs);
  const EfiGuid cert_x509 = EfiGuid::CertX509();
  assert(cert_x509.ToString() == "a5c059a1-94e4-4aa7-87b5-ab155c2bf072");
  assert(*EfiGuid::Parse("a5c059a1-94e4-4aa7-87b5-ab155c2bf072") == cert_x509);

  const EfiGuid cert_pkcs7 = EfiGuid::CertPkcs7();
  assert(cert_pkcs7.ToString() == "4aafd29d-68df-49ee-8aa9-347d375665a7");
  assert(*EfiGuid::Parse("4aafd29d-68df-49ee-8aa9-347d375665a7") == cert_pkcs7);

  constexpr EfiGuid custom = EfiGuid::FromParts(
      0x12345678, 0x1234, 0x5678,
      {0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0});
  assert(custom.ToString() == "12345678-1234-5678-1234-56789abcdef0");

  // Round-trip without dashes
  const StatusOr<EfiGuid> no_dashes =
      EfiGuid::Parse("8be4df6193ca11d2aa0d00e098032b8c");
  assert(no_dashes.ok());
  assert(*no_dashes == global);

  // Random generation checks
  const StatusOr<EfiGuid> random_guid = EfiGuid::GenerateRandom();
  assert(random_guid.ok());
  const std::string text = random_guid->ToString();
  assert(text.size() == 36);
  assert(text[8] == '-' && text[13] == '-' && text[18] == '-' && text[23] == '-');
  // RFC 4122 version 4 check: 3rd field starts with '4'
  assert(text[14] == '4');
  // RFC 4122 variant 1 check: 4th field starts with '8', '9', 'a', or 'b'
  const char variant_char = text[19];
  assert(variant_char == '8' || variant_char == '9' || variant_char == 'a' ||
         variant_char == 'b');
  const StatusOr<EfiGuid> parsed_random = EfiGuid::Parse(text);
  assert(parsed_random.ok());
  assert(*parsed_random == *random_guid);

  // Error cases
  assert(!EfiGuid::Parse("").ok());
  assert(!EfiGuid::Parse("8be4df61-93ca-11d2-aa0d-00e098032b8").ok());
  assert(!EfiGuid::Parse("8be4df61-93ca-11d2-aa0d-00e098032b8ccc").ok());
  assert(!EfiGuid::Parse("8be4df61-93ca-11d2-aa0d-00e098032b8g").ok());
}

void CheckSecretString() {
  SecretString empty;
  assert(empty.Empty());
  assert(empty.Size() == 0);
  assert(empty.Value().empty());
  assert(empty == "");
  assert("" == empty);

  SecretString secret("super-secret-password");
  assert(!secret.Empty());
  assert(secret.Size() == 21);
  assert(secret.Value() == "super-secret-password");
  assert(secret == "super-secret-password");
  assert("super-secret-password" == secret);
  assert(secret != "wrong-password");
  assert(secret != "");
  assert(empty != secret);

  // Clone
  SecretString clone = secret.Clone();
  assert(clone == secret);
  assert(clone.Value() == secret.Value());

  // Assign and Clear
  secret.Assign("new-secret");
  assert(secret == "new-secret");
  assert(secret != clone);
  secret.Clear();
  assert(secret.Empty());
  assert(secret.Size() == 0);

  // Move construction
  SecretString moved(std::move(clone));
  assert(moved == "super-secret-password");
  assert(clone.Empty());
  assert(clone.Size() == 0);

  // Move assignment
  SecretString target;
  target = std::move(moved);
  assert(target == "super-secret-password");
  assert(moved.Empty());
  assert(moved.Size() == 0);

  // WipeString helper
  std::string regular = "temporary-plaintext-password";
  WipeString(&regular);
  assert(regular.empty());
  assert(regular.size() == 0);
  WipeString(nullptr);
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
  puu::engine::CheckFileUtils();
  puu::engine::CheckCreatePipe();
  puu::engine::CheckBlockProbing();
  puu::engine::CheckEfiGuid();
  puu::engine::CheckSecretString();
  std::puts("selfcheck: ok");
  return 0;
}
