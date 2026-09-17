// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "config.h"

#include <sys/utsname.h>

#include <array>
#include <bit>
#include <cerrno>
#include <charconv>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <functional>
#include <ios>
#include <iterator>
#include <limits>
#include <map>
#include <optional>
#include <set>
#include <string>
#include <string_view>
#include <system_error>
#include <tuple>
#include <utility>
#include <vector>

#include <absl/base/no_destructor.h>
#include <absl/status/status.h>
#include <absl/strings/ascii.h>
#include <absl/strings/str_split.h>
#include <absl/strings/string_view.h>
#include <fmt/format.h>
#include <yaml-cpp/exceptions.h>
#include <yaml-cpp/node/convert.h>
#include <yaml-cpp/node/detail/impl.h>
#include <yaml-cpp/node/impl.h>
#include <yaml-cpp/node/iterator.h>
#include <yaml-cpp/node/node.h>
#include <yaml-cpp/node/parse.h>

#include "base/status.h"

namespace puu {
namespace {

constexpr size_t kLabelMaxBytes = 63;
constexpr size_t kDigestHexBytes = 64;
constexpr std::string_view kDigestPrefix = "sha256:";
constexpr size_t kXfsLabelMax = 12;
constexpr size_t kExt4LabelMax = 16;

constexpr uint64_t kLayoutSlackBytes = uint64_t{1} << 30;

bool IsAsciiSpaceOrControl(char ch) {
  const auto byte = static_cast<unsigned char>(ch);
  return byte <= 0x20 || byte == 0x7f;
}

bool IsAsciiAlphanumeric(char ch) {
  return absl::ascii_isalnum(static_cast<unsigned char>(ch));
}

std::optional<char32_t> DecodeUtf8(std::string_view text, size_t& index) {
  const auto lead = static_cast<unsigned char>(text[index]);

  if (lead >= 0xf8) {
    return std::nullopt;
  }
  const size_t extra =
      lead < 0x80 ? 0 : static_cast<size_t>(std::countl_one(lead)) - 1;
  char32_t code_point =
      extra == 0 ? lead : static_cast<char32_t>(lead & (0xffu >> (extra + 1)));
  if (index + extra >= text.size()) {
    return std::nullopt;
  }
  for (size_t offset = 1; offset <= extra; ++offset) {
    const auto byte = static_cast<unsigned char>(text[index + offset]);
    if ((byte & 0xc0) != 0x80) {
      return std::nullopt;
    }
    code_point = static_cast<char32_t>((code_point << 6) |
                                       static_cast<char32_t>(byte & 0x3f));
  }
  index += extra + 1;
  constexpr std::array<char32_t, 4> kShortestForm = {0, 0x80, 0x800, 0x10000};
  if (code_point < kShortestForm[extra]) {
    return std::nullopt;
  }
  if (code_point >= 0xd800 && code_point <= 0xdfff) {
    return std::nullopt;
  }
  return code_point;
}

Status ValidatePosixName(std::string_view text, std::string_view type_name) {
  if (text.empty()) {
    return InvalidArgumentError("{} must not be empty", type_name);
  }

  if (text.size() > kPosixNameMaxBytes) {
    return InvalidArgumentError("{} \"{}\" must be at most {} characters",
                                type_name, text, kPosixNameMaxBytes);
  }
  const char first = text.front();
  if (!absl::ascii_islower(static_cast<unsigned char>(first)) && first != '_') {
    return InvalidArgumentError("invalid {}: \"{}\"", type_name, text);
  }
  for (const char ch : text.substr(1)) {
    const auto byte = static_cast<unsigned char>(ch);
    if (!absl::ascii_islower(byte) && !absl::ascii_isdigit(byte) && ch != '_' &&
        ch != '-') {
      return InvalidArgumentError("invalid {}: \"{}\"", type_name, text);
    }
  }
  return absl::OkStatus();
}

}  // namespace

Status Hostname::Validate(std::string_view text) {
  if (text.empty() || text.size() > kHostnameMaxBytes) {
    return InvalidArgumentError("invalid hostname: \"{}\"", text);
  }
  const absl::string_view hostname(text.data(), text.size());
  for (const absl::string_view label : absl::StrSplit(hostname, '.')) {
    if (label.empty() || label.size() > kLabelMaxBytes ||
        label.front() == '-' || label.back() == '-') {
      return InvalidArgumentError("invalid hostname: \"{}\"", text);
    }
    for (const char ch : label) {
      const bool valid = absl::ascii_isdigit(static_cast<unsigned char>(ch)) ||
                         absl::ascii_islower(static_cast<unsigned char>(ch)) ||
                         ch == '-';
      if (!valid) {
        return InvalidArgumentError("invalid hostname: \"{}\"", text);
      }
    }
  }
  return absl::OkStatus();
}

Status Username::Validate(std::string_view text) {
  return ValidatePosixName(text, kTypeName);
}

Status GroupName::Validate(std::string_view text) {
  return ValidatePosixName(text, kTypeName);
}

Status Digest::Validate(std::string_view text) {
  if (text.substr(0, kDigestPrefix.size()) != kDigestPrefix) {
    return InvalidArgumentError(
        "digest \"{}\" must use the sha256:<hex> format", text);
  }
  const std::string_view hex = text.substr(kDigestPrefix.size());
  if (hex.size() != kDigestHexBytes) {
    return InvalidArgumentError(
        "digest \"{}\" must contain exactly 64 hexadecimal digits", text);
  }
  for (const char ch : hex) {
    if (!absl::ascii_isxdigit(static_cast<unsigned char>(ch))) {
      return InvalidArgumentError(
          "digest \"{}\" must contain exactly 64 hexadecimal digits", text);
    }
  }
  return absl::OkStatus();
}

std::string_view Digest::Hex() const {
  const std::string_view text = value_;
  if (text.size() < kDigestPrefix.size()) {
    return {};
  }
  return text.substr(kDigestPrefix.size());
}

Status ImageRef::Validate(std::string_view text) {
  if (text.empty()) {
    return InvalidArgumentError("image reference must not be empty");
  }
  for (const char ch : text) {
    if (IsAsciiSpaceOrControl(ch)) {
      return InvalidArgumentError("invalid image reference: \"{}\"", text);
    }
  }
  constexpr std::array<std::string_view, 3> kTransports = {
      "dir:", "oci-archive:", "oci:"};
  for (const std::string_view transport : kTransports) {
    if (text.substr(0, transport.size()) != transport) {
      continue;
    }
    if (text.size() == transport.size()) {
      return InvalidArgumentError(
          R"(image reference "{}" is missing a path after "{}")", text,
          transport);
    }
    return absl::OkStatus();
  }
  if (text.front() == ':' || text.front() == '@' || text.front() == '/') {
    return InvalidArgumentError("invalid image reference: \"{}\"", text);
  }
  for (const char ch : text) {
    if (IsAsciiAlphanumeric(ch)) {
      return absl::OkStatus();
    }
  }
  return InvalidArgumentError("invalid image reference: \"{}\"", text);
}

Status KernelArg::Validate(std::string_view text) {
  if (text.empty()) {
    return InvalidArgumentError("kernel argument must not be empty");
  }
  for (const char ch : text) {
    if (IsAsciiSpaceOrControl(ch)) {
      return InvalidArgumentError("invalid kernel argument: \"{}\"", text);
    }
  }
  return absl::OkStatus();
}

Status BrandName::Validate(std::string_view text) {
  if (text.empty()) {
    return InvalidArgumentError("brand name must not be empty");
  }
  size_t index = 0;
  while (index < text.size()) {
    const std::optional<char32_t> code_point = DecodeUtf8(text, index);
    if (!code_point.has_value()) {
      return InvalidArgumentError("brand name \"{}\" is not valid UTF-8", text);
    }
    if (*code_point > 0xffff) {
      return InvalidArgumentError(
          "brand name \"{}\" must contain only Basic Multilingual Plane "
          "characters",
          text);
    }
  }
  return absl::OkStatus();
}

Status SingleLine::Validate(std::string_view text) {
  for (const char ch : text) {
    const auto raw = static_cast<unsigned char>(ch);
    if (raw < 0x20 || raw == 0x7f) {
      return InvalidArgumentError(
          "value \"{}\" must not contain control characters", text);
    }
  }
  return absl::OkStatus();
}

Status Timezone::Validate(std::string_view text) {
  if (text.empty()) {
    return InvalidArgumentError("time zone must not be empty");
  }

  const absl::string_view timezone(text.data(), text.size());
  for (const absl::string_view component : absl::StrSplit(timezone, '/')) {
    if (component.empty() || component == "." || component == "..") {
      return InvalidArgumentError("invalid time zone: \"{}\"", text);
    }
  }
  return absl::OkStatus();
}

Status ServerUrl::Validate(std::string_view text) {
  if (text.empty()) {
    return InvalidArgumentError("server URL must not be empty");
  }
  for (const char ch : text) {
    if (IsAsciiSpaceOrControl(ch)) {
      return InvalidArgumentError("server URL \"{}\" must be a single token",
                                  text);
    }
  }
  return absl::OkStatus();
}

StatusOr<VolumeLabel> VolumeLabel::Parse(std::string_view text,
                                         size_t max_bytes) {
  if (text.empty()) {
    return InvalidArgumentError("volume label must not be empty");
  }
  if (text.size() > max_bytes) {
    return InvalidArgumentError(
        "volume label \"{}\" is {} bytes, exceeding the {}-byte filesystem "
        "label limit",
        text, text.size(), max_bytes);
  }
  VolumeLabel label;
  label.value_ = std::string(text);
  return label;
}

StatusOr<PartitionSize> PartitionSize::Parse(std::string_view text) {
  std::string_view rest = text;
  if (!rest.empty() && rest.front() == '+') {
    rest.remove_prefix(1);
  }

  if (rest.size() < 2) {
    return InvalidArgumentError("unsupported partition size: \"{}\"", text);
  }
  const char unit = rest.back();
  rest.remove_suffix(1);

  uint64_t multiplier = 0;
  switch (unit) {
    case 'K':
      multiplier = uint64_t{1} << 10;
      break;
    case 'M':
      multiplier = uint64_t{1} << 20;
      break;
    case 'G':
      multiplier = uint64_t{1} << 30;
      break;
    case 'T':
      multiplier = uint64_t{1} << 40;
      break;
    case 'P':
      multiplier = uint64_t{1} << 50;
      break;
    default:
      return InvalidArgumentError(R"(unsupported partition unit "{}" in "{}")",
                                  unit, text);
  }

  constexpr uint64_t kMax = std::numeric_limits<uint64_t>::max();
  uint64_t count = 0;

  if (rest.empty() || rest.front() < '0' || rest.front() > '9') {
    return InvalidArgumentError("unsupported partition size: \"{}\"", text);
  }
  const char* const first = rest.begin();
  const char* const end = rest.end();
  const auto [digits_end, error] = std::from_chars(first, end, count);
  if (error != std::errc{} || digits_end != end) {
    return InvalidArgumentError("unsupported partition size: \"{}\"", text);
  }
  if (count > kMax / multiplier) {
    return InvalidArgumentError("partition size is too large: \"{}\"", text);
  }
  const uint64_t bytes = count * multiplier;
  if (bytes == 0) {
    return InvalidArgumentError(
        "partition size \"{}\" must be greater than zero", text);
  }
  return PartitionSize(bytes);
}

StatusOr<Arch> HostArch() {
  utsname info{};
  if (uname(&info) != 0) {
    return ErrnoToStatus(errno, "uname");
  }
  return ParseArch(info.machine);
}

StatusOr<Arch> ParseArch(std::string_view text) {
  if (text == "x86_64") {
    return Arch::kX8664;
  }
  if (text == "aarch64") {
    return Arch::kAarch64;
  }
  return InvalidArgumentError("unsupported architecture: \"{}\"", text);
}

std::string_view ArchName(Arch arch) {
  switch (arch) {
    case Arch::kX8664:
      return "x86_64";
    case Arch::kAarch64:
      return "aarch64";
  }
  return "";
}

std::string_view ArchEfiSuffix(Arch arch) {
  switch (arch) {
    case Arch::kX8664:
      return "x64";
    case Arch::kAarch64:
      return "aa64";
  }
  return "";
}

StatusOr<K3sRole> ParseK3sRole(std::string_view text) {
  if (text == "server") {
    return K3sRole::kServer;
  }
  if (text == "agent") {
    return K3sRole::kAgent;
  }
  return InvalidArgumentError(
      R"(k3s role must be "server" or "agent", got "{}")", text);
}

std::string_view K3sRoleName(K3sRole role) {
  switch (role) {
    case K3sRole::kServer:
      return "server";
    case K3sRole::kAgent:
      return "agent";
  }
  return "";
}

std::string_view UserShellPath(UserShell shell) {
  switch (shell) {
    case UserShell::kBash:
      return "/bin/bash";
    case UserShell::kZsh:
      return "/bin/zsh";
  }
  return "";
}

namespace {

std::string JoinPath(std::string_view path, std::string_view key) {
  if (path.empty()) {
    return std::string(key);
  }
  return fmt::format("{}.{}", path, key);
}

Status UnknownKeyError(std::string_view path) {
  return InvalidArgumentError("unknown configuration key \"{}\"", path);
}

Status FieldError(std::string_view path, std::string_view value,
                  const Status& status) {
  return WithContext(status, fmt::format("{} = \"{}\"", path, value));
}

StatusOr<std::string> ScalarText(std::string_view path,
                                 const YAML::Node& node) {
  if (!node.IsScalar()) {
    return InvalidArgumentError("{} must be a scalar value", path);
  }
  return node.Scalar();
}

template <typename T>
Status ParseInto(std::string_view path, const YAML::Node& node, T* out) {
  PUU_ASSIGN_OR_RETURN(const std::string text, ScalarText(path, node));
  StatusOr<T> parsed = T::Parse(text);
  if (!parsed.ok()) {
    return FieldError(path, text, parsed.status());
  }
  *out = *std::move(parsed);
  return absl::OkStatus();
}

Status ParseInto(std::string_view path, const YAML::Node& node,
                 std::filesystem::path* out) {
  PUU_ASSIGN_OR_RETURN(const std::string text, ScalarText(path, node));
  const std::filesystem::path value(text);

  if (value.empty() || !value.is_absolute()) {
    return FieldError(path, text,
                      InvalidArgumentError("must be an absolute path"));
  }
  *out = value.lexically_normal();
  return absl::OkStatus();
}

Status ParseInto(std::string_view path, const YAML::Node& node, K3sRole* out) {
  PUU_ASSIGN_OR_RETURN(const std::string text, ScalarText(path, node));
  StatusOr<K3sRole> role = ParseK3sRole(text);
  if (!role.ok()) {
    return FieldError(path, text, role.status());
  }
  *out = *role;
  return absl::OkStatus();
}

Status ParseLabelInto(std::string_view path, const YAML::Node& node,
                      size_t max_bytes, VolumeLabel* out) {
  PUU_ASSIGN_OR_RETURN(const std::string text, ScalarText(path, node));
  StatusOr<VolumeLabel> label = VolumeLabel::Parse(text, max_bytes);
  if (!label.ok()) {
    return FieldError(path, text, label.status());
  }
  *out = *std::move(label);
  return absl::OkStatus();
}

Status ParseLabelInto(std::string_view path, const YAML::Node& node,
                      size_t max_bytes, std::optional<VolumeLabel>* out) {
  if (node.IsNull()) {
    out->reset();
    return absl::OkStatus();
  }
  VolumeLabel label;
  PUU_RETURN_IF_ERROR(ParseLabelInto(path, node, max_bytes, &label));
  *out = std::move(label);
  return absl::OkStatus();
}

template <typename T>
Status ParseOptionalInto(std::string_view path, const YAML::Node& node,
                         std::optional<T>* out) {
  if (node.IsNull()) {
    out->reset();
    return absl::OkStatus();
  }
  PUU_ASSIGN_OR_RETURN(const std::string text, ScalarText(path, node));
  const absl::string_view trimmed_absl = absl::StripAsciiWhitespace(text);
  const std::string_view trimmed(trimmed_absl.data(), trimmed_absl.size());
  if (trimmed.empty()) {
    out->reset();
    return absl::OkStatus();
  }
  StatusOr<T> parsed = T::Parse(trimmed);
  if (!parsed.ok()) {
    return FieldError(path, trimmed, parsed.status());
  }
  *out = *std::move(parsed);
  return absl::OkStatus();
}

template <typename Handler>
Status ForEachKey(std::string_view path, const YAML::Node& node,
                  Handler handler) {
  if (!node.IsDefined() || node.IsNull()) {
    return absl::OkStatus();
  }
  if (!node.IsMap()) {
    return InvalidArgumentError("{} must be a mapping",
                                path.empty() ? "configuration" : path);
  }
  for (const auto& entry : node) {
    if (!entry.first.IsScalar()) {
      return InvalidArgumentError("{} has a non-scalar key",
                                  path.empty() ? "configuration" : path);
    }
    const std::string key = entry.first.Scalar();
    PUU_RETURN_IF_ERROR(handler(key, entry.second, JoinPath(path, key)));
  }
  return absl::OkStatus();
}

template <typename T>
Status ParseInto(std::string_view path, const YAML::Node& node,
                 std::optional<T>* out) {
  return ParseOptionalInto(path, node, out);
}

Status ParseInto(std::string_view path, const YAML::Node& node,
                 std::vector<GroupName>* out) {
  if (!node.IsDefined() || node.IsNull()) {
    out->clear();
    return absl::OkStatus();
  }
  if (!node.IsSequence()) {
    return InvalidArgumentError("{} must be a sequence", path);
  }
  std::vector<GroupName> groups;
  groups.reserve(node.size());
  for (size_t index = 0; index < node.size(); ++index) {
    GroupName group;
    PUU_RETURN_IF_ERROR(
        ParseInto(fmt::format("{}[{}]", path, index), node[index], &group));
    groups.push_back(std::move(group));
  }
  *out = std::move(groups);
  return absl::OkStatus();
}

Status ParseInto(std::string_view path, const YAML::Node& node,
                 std::map<std::string, KernelArg>* out) {
  std::map<std::string, KernelArg> consoles;
  PUU_RETURN_IF_ERROR(
      ForEachKey(path, node,
                 [&consoles](std::string_view key, const YAML::Node& value,
                             const std::string& field) -> Status {
                   StatusOr<Arch> arch = ParseArch(key);
                   if (!arch.ok()) return UnknownKeyError(field);
                   KernelArg argument;
                   PUU_RETURN_IF_ERROR(ParseInto(field, value, &argument));
                   consoles.insert_or_assign(std::string(ArchName(*arch)),
                                             std::move(argument));
                   return absl::OkStatus();
                 }));
  *out = std::move(consoles);
  return absl::OkStatus();
}

template <typename Struct>
struct FieldEntry {
  std::string_view key;
  std::string_view def;
  std::function<Status(const std::string& path, const YAML::Node& node,
                       Struct* out)>
      handle;
};

template <typename Struct, typename Member>
FieldEntry<Struct> BindField(std::string_view key, Member Struct::* member,
                             std::string_view def = "") {
  return {
      key, def,
      [member](const std::string& path, const YAML::Node& node, Struct* out) {
        return ParseInto(path, node, &(out->*member));
      }};
}

template <size_t MaxBytes, typename Struct, typename Member>
FieldEntry<Struct> BindField(std::string_view key, Member Struct::* member,
                             std::string_view def) {
  return {
      key, def,
      [member](const std::string& path, const YAML::Node& node, Struct* out) {
        return ParseLabelInto(path, node, MaxBytes, &(out->*member));
      }};
}

template <typename Struct, size_t N>
Status ParseFields(std::string_view path, const YAML::Node& node, Struct* out,
                   const std::array<FieldEntry<Struct>, N>& fields) {
  return ForEachKey(
      path, node,
      [&fields, out](std::string_view key, const YAML::Node& value,
                     const std::string& field) -> Status {
        for (const FieldEntry<Struct>& entry : fields) {
          if (entry.key == key) {
            return entry.handle(field, value, out);
          }
        }
        return UnknownKeyError(field);
      });
}

template <typename Struct, size_t N>
struct SectionEntry {
  std::string_view key;
  Struct Config::* section;
  std::array<FieldEntry<Struct>, N> fields;

  Status Parse(std::string_view path, const YAML::Node& node,
               Config* out) const {
    return ParseFields(path, node, &(out->*section), fields);
  }

  void ApplyDefaults(Config* out) const {
    for (const FieldEntry<Struct>& field : fields) {
      if (field.def.empty()) {
        continue;
      }

      const Status status = field.handle(
          "defaults", YAML::Load(std::string(field.def)), &(out->*section));
      if (!status.ok()) {
        std::abort();
      }
    }
  }
};

const auto& Sections() {
  static const absl::NoDestructor sections(std::tuple{
      SectionEntry{.key = "branding",
                   .section = &Config::branding,
                   .fields = std::array{BindField("name", &BrandingConfig::name,
                                                  "Puu")}},
      SectionEntry{
          .key = "partitions",
          .section = &Config::partitions,
          .fields =
              std::array{
                  BindField("efi", &PartitionsConfig::efi, "512M"),
                  BindField("rootMin", &PartitionsConfig::root_min, "64G"),
                  BindField("home", &PartitionsConfig::home, "256G")}},
      SectionEntry{
          .key = "labels",
          .section = &Config::labels,
          .fields = std::array{BindField<kXfsLabelMax>(
                                   "root", &LabelsConfig::root, "puu-root"),
                               BindField<kExt4LabelMax>(
                                   "home", &LabelsConfig::home, "puu-home")}},
      SectionEntry{
          .key = "image",
          .section = &Config::image,
          .fields =
              std::array{
                  BindField("targetImgRef", &ImageConfig::target_imgref,
                            "quay.io/puu/puu:latest"),
                  BindField("expectedDigest", &ImageConfig::expected_digest),
                  BindField<kExt4LabelMax>(
                      "sourceLabel", &ImageConfig::source_label, "PAYLOAD"),
                  BindField("sourceMount", &ImageConfig::source_mount,
                            "/run/puu-installer/payload")}},
      SectionEntry{
          .key = "identity",
          .section = &Config::identity,
          .fields =
              std::array{
                  BindField("keymap", &IdentityConfig::keymap, "us"),
                  BindField("locale", &IdentityConfig::locale, "C.UTF-8"),
                  BindField("timezone", &IdentityConfig::timezone, "UTC"),
                  BindField("hostname", &IdentityConfig::hostname, "puu")}},
      SectionEntry{
          .key = "flatpak",
          .section = &Config::flatpak,
          .fields =
              std::array{
                  BindField("manifestPath", &FlatpakConfig::manifest_path,
                            "/usr/share/puu-os/flatpaks.list"),
                  BindField("syncHelper", &FlatpakConfig::sync_helper,
                            "/usr/libexec/puu-os/flatpak-sync"),
                  BindField("liveStore", &FlatpakConfig::live_store,
                            "/var/lib/flatpak")}},
      SectionEntry{
          .key = "models",
          .section = &Config::models,
          .fields =
              std::array{BindField("syncHelper", &ModelsConfig::sync_helper,
                                   "/usr/libexec/puu-os/preseed-vllm-models"),
                         BindField("liveStore", &ModelsConfig::live_store,
                                   "/var/lib/vllm/models")}},
      SectionEntry{
          .key = "k3s",
          .section = &Config::k3s,
          .fields =
              std::array{BindField("helper", &K3sConfig::helper,
                                   "/usr/libexec/puu-os/configure-k3s-cluster"),
                         BindField("role", &K3sConfig::role, "server"),
                         BindField("serverUrl", &K3sConfig::server_url),
                         BindField("nodeName", &K3sConfig::node_name)}},
      SectionEntry{
          .key = "homed",
          .section = &Config::homed,
          .fields = std::array{BindField(
              "groups", &HomedConfig::groups,
              "[adm, audio, cdrom, dip, input, lpadmin, netdev, "
              "plugdev, render, sudo, video, wheel, realtime, uucp]")}}});
  return *sections;
}

}  // namespace

Config Config::Defaults() {
  Config config;
  std::apply(
      [&config](const auto&... sections) {
        (sections.ApplyDefaults(&config), ...);
      },
      Sections());
  return config;
}

namespace {

Status RejectDuplicateKeys(std::string_view path, const YAML::Node& node) {
  if (node.IsMap()) {
    std::set<std::string> seen;
    for (const auto& item : node) {
      const auto key = item.first.as<std::string>();
      if (!seen.insert(key).second) {
        return InvalidArgumentError("duplicate key \"{}{}\"", path, key);
      }
      PUU_RETURN_IF_ERROR(
          RejectDuplicateKeys(fmt::format("{}{}.", path, key), item.second));
    }
  } else if (node.IsSequence()) {
    for (size_t index = 0; index < node.size(); ++index) {
      PUU_RETURN_IF_ERROR(
          RejectDuplicateKeys(fmt::format("{}[{}]", path, index), node[index]));
    }
  }
  return absl::OkStatus();
}

}  // namespace

StatusOr<Config> Config::ParseYaml(std::string_view yaml) {
  YAML::Node root;
  try {
    std::vector<YAML::Node> documents = YAML::LoadAll(std::string(yaml));
    if (documents.size() > 1) {
      return InvalidArgumentError(
          "configuration must contain exactly one YAML document");
    }
    if (!documents.empty()) {
      root = documents.front();
    }
  } catch (const YAML::Exception& error) {
    return InvalidArgumentError("invalid YAML: {}", error.what());
  }

  if (!root.IsMap()) {
    return InvalidArgumentError("configuration must be a mapping");
  }
  PUU_RETURN_IF_ERROR(RejectDuplicateKeys("", root));

  Config config = Defaults();
  PUU_RETURN_IF_ERROR(ForEachKey(
      "", root,
      [&config](std::string_view key, const YAML::Node& value,
                const std::string& field) -> Status {
        if (key == "serialConsole") {
          return ParseInto(field, value, &config.serial_console);
        }
        std::optional<Status> matched;
        std::apply(
            [&config, &matched, key, &value, &field](const auto&... sections) {
              ((key == sections.key
                    ? (matched = sections.Parse(field, value, &config), true)
                    : false) ||
               ...);
            },
            Sections());
        return matched.has_value() ? *matched : UnknownKeyError(field);
      }));

  constexpr uint64_t kMiB = 1 << 20;
  constexpr uint64_t kGiB = 1 << 30;
  struct SizeFloor {
    std::string_view field;
    uint64_t configured;
    uint64_t minimum;
  };
  const std::array floors{
      SizeFloor{.field = "partitions.efi",
                .configured = config.partitions.efi.Bytes(),
                .minimum = 64 * kMiB},
      SizeFloor{.field = "partitions.rootMin",
                .configured = config.partitions.root_min.Bytes(),
                .minimum = 2 * kGiB},
      SizeFloor{.field = "partitions.home",
                .configured = config.partitions.home.Bytes(),
                .minimum = kMiB}};
  for (const SizeFloor& floor : floors) {
    if (floor.configured < floor.minimum) {
      return InvalidArgumentError(
          "{} is {} bytes but must be at least {} bytes", floor.field,
          floor.configured, floor.minimum);
    }
  }

  return config;
}

StatusOr<Config> Config::Load(const std::filesystem::path& path) {
  std::ifstream file(path, std::ios::binary);
  if (!file) {
    return InvalidArgumentError("failed to read config {}", path.string());
  }
  std::string const text{std::istreambuf_iterator<char>(file),
                         std::istreambuf_iterator<char>()};

  if (file.bad()) {
    return InvalidArgumentError("failed to read config {}", path.string());
  }
  StatusOr<Config> config = ParseYaml(text);
  if (!config.ok()) {
    return WithContext(config.status(),
                       fmt::format("failed to parse config {}", path.string()));
  }
  return config;
}

StatusOr<uint64_t> Config::MinTargetSizeBytes() const {
  constexpr uint64_t kMax = std::numeric_limits<uint64_t>::max();
  const std::array<uint64_t, 4> parts = {
      partitions.efi.Bytes(), partitions.root_min.Bytes(),
      partitions.home.Bytes(), kLayoutSlackBytes};
  uint64_t total = 0;
  for (const uint64_t part : parts) {
    if (part > kMax - total) {
      return InvalidArgumentError(
          "partition sizes overflow the addressable range of a drive");
    }
    total += part;
  }
  return total;
}

}  // namespace puu
