// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026
//
// Installer configuration schema. Every field that reaches a syscall, a
// filesystem label, or a kernel command line is parsed into a validated type,
// so the rest of the engine never re-checks external input.

#ifndef PUU_CONFIG_H_
#define PUU_CONFIG_H_

#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <map>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "base/status.h"

namespace puu {

// RFC 1123 allows 253 bytes, but sethostname(2) enforces HOST_NAME_MAX (64)
// and a hostname that passes validation but fails at boot leaves an unbootable
// system.
inline constexpr size_t kHostnameMaxBytes = 64;
inline constexpr size_t kPosixNameMaxBytes = 32;

// Base for strings that carry a validated invariant. `Derived` supplies
// `static Status Validate(std::string_view)` and a `kTypeName`.
template <typename Derived>
class ValidatedString {
  // Private so the CRTP base cannot be instantiated or inherited directly.
  ValidatedString() = default;
  friend Derived;

 public:
  static StatusOr<Derived> Parse(std::string_view text) {
    PUU_RETURN_IF_ERROR(Derived::Validate(text));
    Derived value;
    value.value_ = std::string(text);
    return value;
  }

  [[nodiscard]] const std::string& Value() const { return value_; }
  [[nodiscard]] bool Empty() const { return value_.empty(); }
  friend bool operator==(const Derived& lhs, const Derived& rhs) {
    return lhs.value_ == rhs.value_;
  }

 private:
  std::string value_;
};

// A DNS hostname: 1-253 bytes, dot-separated labels of 1-63 bytes drawn from
// [A-Za-z0-9-] with no leading or trailing hyphen.
class Hostname : public ValidatedString<Hostname> {
 public:
  static constexpr std::string_view kTypeName = "hostname";
  static Status Validate(std::string_view text);
};

// A POSIX user name: [a-z_][a-z0-9_-]*.
class Username : public ValidatedString<Username> {
 public:
  static constexpr std::string_view kTypeName = "user name";
  static Status Validate(std::string_view text);
};

// A POSIX group name, same grammar as Username.
class GroupName : public ValidatedString<GroupName> {
 public:
  static constexpr std::string_view kTypeName = "group name";
  static Status Validate(std::string_view text);
};

// "sha256:" followed by exactly 64 hex digits.
class Digest : public ValidatedString<Digest> {
 public:
  static constexpr std::string_view kTypeName = "digest";
  static Status Validate(std::string_view text);

  // The 64-character hex body, without the "sha256:" prefix.
  [[nodiscard]] std::string_view Hex() const;
};

// A container image reference, optionally prefixed with a transport
// ("dir:", "oci-archive:", "oci:").
class ImageRef : public ValidatedString<ImageRef> {
 public:
  static constexpr std::string_view kTypeName = "image reference";
  static Status Validate(std::string_view text);
};

// A single kernel command-line token: non-empty, no whitespace or controls.
class KernelArg : public ValidatedString<KernelArg> {
 public:
  static constexpr std::string_view kTypeName = "kernel argument";
  static Status Validate(std::string_view text);
};

// Product name shown in the UI and used as the EFI boot entry description.
// Restricted to the Basic Multilingual Plane because it is rendered on a
// Linux virtual console.
class BrandName : public ValidatedString<BrandName> {
 public:
  static constexpr std::string_view kTypeName = "brand name";
  static Status Validate(std::string_view text);
};

// A string that must not contain CR or LF. May be empty.
class SingleLine : public ValidatedString<SingleLine> {
 public:
  static constexpr std::string_view kTypeName = "single-line string";
  static Status Validate(std::string_view text);
};

// An IANA time zone name. Rejects any component that could escape the
// zoneinfo directory.
class Timezone : public ValidatedString<Timezone> {
 public:
  static constexpr std::string_view kTypeName = "time zone";
  static Status Validate(std::string_view text);
};

// A k3s server URL: non-empty and free of whitespace.
class ServerUrl : public ValidatedString<ServerUrl> {
 public:
  static constexpr std::string_view kTypeName = "server URL";
  static Status Validate(std::string_view text);
};

// A filesystem volume label. `max_bytes` differs per filesystem: XFS allows 12
// bytes, ext4 and FAT allow 16.
class VolumeLabel {
 public:
  VolumeLabel() = default;

  static StatusOr<VolumeLabel> Parse(std::string_view text, size_t max_bytes);

  [[nodiscard]] const std::string& Value() const { return value_; }

 private:
  std::string value_;
};

// A size in bytes written as <count><unit>, where unit is one of K, M, G, T, P
// interpreted as powers of 1024. A leading '+' is accepted and ignored.
class PartitionSize {
 public:
  PartitionSize() = default;
  explicit PartitionSize(uint64_t bytes) : bytes_(bytes) {}

  static StatusOr<PartitionSize> Parse(std::string_view text);

  [[nodiscard]] uint64_t Bytes() const { return bytes_; }

 private:
  uint64_t bytes_ = 0;
};

// Target CPU architecture. Only the architectures the installer can produce a
// bootable ESP for are represented.
enum class Arch : std::uint8_t { kX8664, kAarch64 };

StatusOr<Arch> HostArch();
StatusOr<Arch> ParseArch(std::string_view text);
std::string_view ArchName(Arch arch);
// "x64" or "aa64": the EFI removable-media boot file suffix.
std::string_view ArchEfiSuffix(Arch arch);

enum class K3sRole : std::uint8_t { kServer, kAgent };

StatusOr<K3sRole> ParseK3sRole(std::string_view text);
std::string_view K3sRoleName(K3sRole role);

// Login shell offered by the installer UI. Not configurable via YAML.
enum class UserShell : std::uint8_t { kBash, kZsh };

std::string_view UserShellPath(UserShell shell);

struct BrandingConfig {
  BrandName name;
};

struct PartitionsConfig {
  PartitionSize efi;
  PartitionSize root_min;
  PartitionSize home;
};

struct LabelsConfig {
  VolumeLabel root;  // XFS, at most 12 bytes.
  VolumeLabel home;  // ext4, at most 16 bytes.
};

struct ImageConfig {
  ImageRef target_imgref;
  std::optional<Digest> expected_digest;
  std::optional<VolumeLabel> source_label;  // at most 16 bytes.
  std::filesystem::path source_mount;
};

struct IdentityConfig {
  SingleLine keymap;
  SingleLine locale;
  Timezone timezone;
  Hostname hostname;
};

struct FlatpakConfig {
  // Ref list consulted inside the target to decide whether pre-seeding has any
  // work to do. The sync helper reads the same file at its own default path.
  std::filesystem::path manifest_path;
  std::filesystem::path sync_helper;
  std::filesystem::path live_store;
};

struct ModelsConfig {
  std::filesystem::path sync_helper;
  std::filesystem::path live_store;
};

struct K3sConfig {
  K3sRole role = K3sRole::kServer;
  std::optional<ServerUrl> server_url;
  std::optional<Hostname> node_name;
};

struct HomedConfig {
  std::vector<GroupName> groups;
};

// Full installer configuration. Unknown YAML keys are rejected so that a
// misspelled key never silently falls back to a default.
struct Config {
  static Config Defaults();

  // Parses YAML text. The caller owns error reporting; no partial config is
  // returned on failure.
  static StatusOr<Config> ParseYaml(std::string_view yaml);

  static StatusOr<Config> Load(const std::filesystem::path& path);

  // Smallest target drive the layout fits on: efi + rootMin + home + 1 GiB of
  // GPT and alignment slack.
  [[nodiscard]] StatusOr<uint64_t> MinTargetSizeBytes() const;

  BrandingConfig branding;
  PartitionsConfig partitions;
  LabelsConfig labels;
  ImageConfig image;
  IdentityConfig identity;
  FlatpakConfig flatpak;
  ModelsConfig models;
  K3sConfig k3s;
  HomedConfig homed;
  // Architecture name -> kernel console argument.
  std::map<std::string, KernelArg> serial_console;
};

}  // namespace puu

#endif  // PUU_CONFIG_H_
