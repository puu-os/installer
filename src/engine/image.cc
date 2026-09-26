// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/image.h"

#include <sys/mount.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <charconv>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <ios>
#include <limits>
#include <memory>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <absl/status/status.h>
#include <absl/strings/match.h>
#include <fmt/format.h>
#include <nlohmann/json.hpp>

#include "base/status.h"
#include "config.h"
#include "engine/block.h"
#include "engine/pipeline.h"
#include "engine/util.h"

namespace puu::engine {
namespace {

constexpr std::string_view kManifestFilename = "image.manifest";

constexpr std::string_view kImageFilename = "image.tar";

constexpr uint64_t kLayerExpansionNumerator = 22;
constexpr uint64_t kLayerExpansionDenominator = 10;

constexpr size_t kTarBlockSize = 512;

constexpr uint64_t kMaxEntryBytes = 64ULL << 20;

constexpr size_t kTarNameOffset = 0;
constexpr size_t kTarNameSize = 100;
constexpr size_t kTarSizeOffset = 124;
constexpr size_t kTarSizeWidth = 12;
constexpr size_t kTarChecksumOffset = 148;
constexpr size_t kTarChecksumWidth = 8;
constexpr size_t kTarMagicOffset = 257;
constexpr size_t kTarPrefixOffset = 345;
constexpr size_t kTarPrefixSize = 155;

using TarBlock = std::array<char, kTarBlockSize>;

StatusOr<nlohmann::json> ParseJson(std::span<const uint8_t> data,
                                   std::string_view what) {
  nlohmann::json json =
      nlohmann::json::parse(data.begin(), data.end(), nullptr, false);
  if (json.is_discarded()) {
    return InvalidArgumentError("{} is not valid JSON", what);
  }
  return json;
}

const nlohmann::json* Member(const nlohmann::json& json, std::string_view key) {
  if (!json.is_object()) {
    return nullptr;
  }
  const auto found = json.find(key);
  if (found == json.end()) {
    return nullptr;
  }
  return &*found;
}

StatusOr<std::optional<nlohmann::json>> ReadSidecar(
    const std::filesystem::path& source_image) {
  const std::filesystem::path path =
      source_image.parent_path() / kManifestFilename;
  if (!Exists(path)) {
    return std::nullopt;
  }
  if (!IsRegularFile(path)) {
    return FailedPreconditionError("image manifest '{}' is not a regular file",
                                   path.string());
  }

  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> data, ReadFileBytes(path));
  PUU_ASSIGN_OR_RETURN(nlohmann::json json, ParseJson(data, path.string()));
  return json;
}

StatusOr<uint64_t> ParseDecimal(std::string_view text, std::string_view what) {
  uint64_t value = 0;
  const auto [end, error] =
      std::from_chars(text.data(), text.data() + text.size(), value);
  if (error != std::errc() || end != text.data() + text.size()) {
    return InvalidArgumentError("{} is not a decimal byte count: \"{}\"", what,
                                text);
  }
  return value;
}

StatusOr<uint64_t> ParseOctalField(std::string_view field,
                                   std::string_view what) {
  const size_t start = field.find_first_not_of(' ');
  if (start == std::string_view::npos) {
    return uint64_t{0};
  }
  uint64_t value = 0;
  for (size_t index = start; index < field.size(); ++index) {
    const char ch = field[index];
    if (ch == '\0' || ch == ' ') {
      break;
    }
    if (ch < '0' || ch > '7') {
      return InvalidArgumentError("tar header has a malformed {} field", what);
    }
    if (value > (std::numeric_limits<uint64_t>::max() - 7) / 8) {
      return InvalidArgumentError("tar header has an out-of-range {} field",
                                  what);
    }
    value = (value * 8) + static_cast<uint64_t>(ch - '0');
  }
  return value;
}

StatusOr<uint64_t> ParseEntrySize(const TarBlock& block) {
  const std::string_view field(&block[kTarSizeOffset], kTarSizeWidth);
  const auto first = static_cast<uint8_t>(field.front());
  if ((first & 0x80) == 0) {
    return ParseOctalField(field, "size");
  }

  if ((first & 0x7f) != 0) {
    return InvalidArgumentError("tar entry is larger than 2^64 bytes");
  }
  uint64_t value = 0;
  for (size_t index = 1; index < field.size(); ++index) {
    const auto byte = static_cast<uint8_t>(field[index]);

    if (index < field.size() - sizeof(uint64_t) && byte != 0) {
      return InvalidArgumentError("tar entry is larger than 2^64 bytes");
    }
    value = (value << 8) | byte;
  }
  return value;
}

bool ChecksumMatches(const TarBlock& block, uint64_t recorded) {
  uint64_t unsigned_sum = 0;
  int64_t signed_sum = 0;
  for (size_t index = 0; index < block.size(); ++index) {
    const bool in_checksum = index >= kTarChecksumOffset &&
                             index < kTarChecksumOffset + kTarChecksumWidth;
    const char ch = in_checksum ? ' ' : block[index];
    unsigned_sum += static_cast<uint8_t>(ch);
    signed_sum += ch;
  }
  return recorded == unsigned_sum ||
         (signed_sum >= 0 && recorded == static_cast<uint64_t>(signed_sum));
}

std::string EntryName(const TarBlock& block) {
  const std::string_view name_field(&block[kTarNameOffset], kTarNameSize);
  std::string_view name = name_field.substr(0, name_field.find('\0'));

  std::string_view prefix;
  const std::string_view magic(&block[kTarMagicOffset], 5);
  if (magic == "ustar") {
    const std::string_view prefix_field(&block[kTarPrefixOffset],
                                        kTarPrefixSize);
    prefix = prefix_field.substr(0, prefix_field.find('\0'));
  }

  std::string path =
      prefix.empty() ? std::string(name) : fmt::format("{}/{}", prefix, name);

  if (path.starts_with("./")) {
    path.erase(0, 2);
  }
  return path;
}

StatusOr<std::optional<std::vector<uint8_t>>> ReadArchiveEntry(
    const std::filesystem::path& archive, std::string_view name) {
  std::ifstream file(archive, std::ios::binary);
  if (!file.is_open()) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("open {}", archive.string()));
  }

  while (true) {
    TarBlock block = {};
    file.read(block.data(), static_cast<std::streamsize>(block.size()));
    if (file.gcount() == 0 && file.eof()) {
      return std::nullopt;
    }
    if (file.bad()) {
      const int error = errno;
      return ErrnoToStatus(error, fmt::format("read {}", archive.string()));
    }
    if (file.gcount() != static_cast<std::streamsize>(block.size())) {
      return InvalidArgumentError("{} ends inside a tar header",
                                  archive.string());
    }

    if (std::ranges::all_of(block, [](char ch) { return ch == '\0'; })) {
      return std::nullopt;
    }

    PUU_ASSIGN_OR_RETURN(
        const uint64_t checksum,
        ParseOctalField(
            std::string_view(&block[kTarChecksumOffset], kTarChecksumWidth),
            "checksum"));
    if (!ChecksumMatches(block, checksum)) {
      return InvalidArgumentError("{} has a corrupt tar header",
                                  archive.string());
    }

    PUU_ASSIGN_OR_RETURN(const uint64_t size, ParseEntrySize(block));
    const std::string entry = EntryName(block);

    if (entry == name) {
      if (size > kMaxEntryBytes) {
        return InvalidArgumentError(
            "{} in {} is {} bytes, too large to be a manifest", name,
            archive.string(), size);
      }
      std::vector<uint8_t> data(static_cast<size_t>(size));
      file.read(reinterpret_cast<char*>(data.data()),
                static_cast<std::streamsize>(size));
      if (file.gcount() != static_cast<std::streamsize>(size)) {
        return InvalidArgumentError("{} in {} is truncated", name,
                                    archive.string());
      }
      return data;
    }

    if (size > std::numeric_limits<uint64_t>::max() - kTarBlockSize) {
      return InvalidArgumentError("tar entry size overflows the archive");
    }
    const uint64_t padded =
        (size + kTarBlockSize - 1) / kTarBlockSize * kTarBlockSize;
    if (padded >
        static_cast<uint64_t>(std::numeric_limits<std::streamoff>::max())) {
      return InvalidArgumentError("tar entry size overflows the archive");
    }
    file.seekg(static_cast<std::streamoff>(padded), std::ios::cur);
    if (!file) {
      return InvalidArgumentError("{} ends inside a tar entry",
                                  archive.string());
    }
  }
}

StatusOr<std::string> ManifestBlobPath(
    std::span<const uint8_t> index, const std::filesystem::path& archive,
    const std::optional<std::string>& image_name) {
  PUU_ASSIGN_OR_RETURN(
      const nlohmann::json json,
      ParseJson(index, fmt::format("index.json in {}", archive.string())));
  const nlohmann::json* manifests = Member(json, "manifests");
  if (manifests == nullptr || !manifests->is_array() || manifests->empty()) {
    return InvalidArgumentError("index.json in {} lists no manifests",
                                archive.string());
  }

  const nlohmann::json* selected = &manifests->front();
  if (image_name.has_value()) {
    selected = nullptr;
    for (const nlohmann::json& descriptor : *manifests) {
      const nlohmann::json* annotations = Member(descriptor, "annotations");
      const nlohmann::json* name =
          annotations == nullptr
              ? nullptr
              : Member(*annotations, "org.opencontainers.image.ref.name");
      if (name != nullptr && name->is_string() &&
          name->get_ref<const std::string&>() == *image_name) {
        selected = &descriptor;
        break;
      }
    }
    if (selected == nullptr) {
      return NotFoundError("index.json in {} has no image named '{}'",
                           archive.string(), *image_name);
    }
  }

  const nlohmann::json* digest = Member(*selected, "digest");
  if (digest == nullptr || !digest->is_string()) {
    return InvalidArgumentError("index.json in {} has no manifest digest",
                                archive.string());
  }
  PUU_ASSIGN_OR_RETURN(const Digest parsed,
                       Digest::Parse(digest->get<std::string>()));
  return fmt::format("blobs/sha256/{}", parsed.Hex());
}


StatusOr<uint64_t> LayerTotalBytes(std::span<const uint8_t> raw_manifest,
                                   const std::filesystem::path& source_image) {
  PUU_ASSIGN_OR_RETURN(
      const nlohmann::json json,
      ParseJson(raw_manifest,
                fmt::format("image manifest in {}", source_image.string())));
  const nlohmann::json* layers = Member(json, "layers");
  if (layers == nullptr || !layers->is_array()) {
    return InvalidArgumentError("image manifest in {} has no layers",
                                source_image.string());
  }
  uint64_t total = 0;
  for (const nlohmann::json& layer : *layers) {
    const nlohmann::json* size = Member(layer, "size");
    if (size == nullptr || !size->is_number_unsigned()) {
      continue;
    }
    const auto bytes = size->get<uint64_t>();
    if (total > std::numeric_limits<uint64_t>::max() - bytes) {
      return InvalidArgumentError("image manifest in {} has absurd layer sizes",
                                  source_image.string());
    }
    total += bytes;
  }
  if (total == 0) {
    return InvalidArgumentError("image manifest in {} has no layer sizes",
                                source_image.string());
  }
  return total;
}

}  // namespace

StatusOr<ImageManifest> LoadImageManifest(
    const std::filesystem::path& source_image) {
  ImageManifest manifest;
  PUU_ASSIGN_OR_RETURN(const std::optional<nlohmann::json> json,
                       ReadSidecar(source_image));
  if (!json.has_value()) {
    return manifest;
  }

  const std::filesystem::path sidecar =
      source_image.parent_path() / kManifestFilename;

  if (const nlohmann::json* target = Member(*json, "target_imgref");
      target != nullptr && target->is_string()) {
    PUU_ASSIGN_OR_RETURN(ImageRef parsed,
                         ImageRef::Parse(target->get<std::string>()));
    manifest.target_imgref = std::move(parsed);
  }

  if (const nlohmann::json* version = Member(*json, "version");
      version != nullptr && version->is_string()) {
    std::string text = version->get<std::string>();
    if (!text.empty()) {
      manifest.version = std::move(text);
    }
  }

  if (const nlohmann::json* size = Member(*json, "installSize");
      size != nullptr) {
    if (size->is_number_unsigned()) {
      manifest.install_size = size->get<uint64_t>();
    } else if (size->is_string()) {
      PUU_ASSIGN_OR_RETURN(
          const uint64_t bytes,
          ParseDecimal(size->get<std::string>(),
                       fmt::format("installSize in {}", sidecar.string())));
      manifest.install_size = bytes;
    } else {
      return InvalidArgumentError("installSize in {} is not a byte count",
                                  sidecar.string());
    }
  }

  if (const nlohmann::json* imgref = Member(*json, "imgref");
      imgref != nullptr && imgref->is_string()) {
    std::string_view name = imgref->get_ref<const std::string&>();

    if (name.starts_with("oci:")) {
      name.remove_prefix(4);
    }
    if (!name.empty()) {
      PUU_ASSIGN_OR_RETURN(ImageRef parsed, ImageRef::Parse(fmt::format(
                                                "oci-archive:{}:{}",
                                                source_image.string(), name)));
      manifest.source_imgref = std::move(parsed);
      manifest.archive_image_name = std::string(name);
    }
  }

  return manifest;
}

StatusOr<std::optional<std::filesystem::path>> DefaultSourceImage(
    const Config& config) {
  const std::filesystem::path& mount_point = config.image.source_mount;
  std::filesystem::path image = mount_point / kImageFilename;

  if (IsRegularFile(image)) {
    return image;
  }

  if (!config.image.source_label.has_value()) {
    return NotFoundError(
        "no local bootc source image at {}; "
        "image.sourceLabel fallback is disabled",
        image.string());
  }

  PUU_ASSIGN_OR_RETURN(const Mounts mounts, Mounts::Read());
  if (mounts.IsMountPoint(mount_point)) {
    PUU_RETURN_IF_ERROR(Unmount(mount_point));
  }

  if (RunningInContainer()) {
    return std::nullopt;
  }

  PUU_ASSIGN_OR_RETURN(const std::optional<std::filesystem::path> device,
                       FindDeviceByLabel(config.image.source_label->Value()));
  if (!device.has_value()) {
    return NotFoundError(
        "no filesystem labelled \"{}\" holds the install image",
        config.image.source_label->Value());
  }
  PUU_ASSIGN_OR_RETURN(const std::string filesystem_type,
                       ProbeFilesystemType(*device));

  PUU_RETURN_IF_ERROR(MountTarget(*device, mount_point, filesystem_type,
                                  "ro,nosuid,nodev,noexec"));

  if (!IsRegularFile(image)) {
    Unmount(mount_point).IgnoreError();
    return NotFoundError("{} holds no {}", device->string(), kImageFilename);
  }
  return image;
}

StatusOr<ImageRef> LoadTargetImgRef(const ImageManifest& manifest,
                                    const Config& config) {
  if (manifest.target_imgref.has_value()) {
    return *manifest.target_imgref;
  }
  if (!config.image.target_imgref.Empty()) {
    return config.image.target_imgref;
  }
  return InvalidArgumentError("target image reference is empty");
}

StatusOr<std::vector<uint8_t>> RawManifest(
    const std::filesystem::path& source_image, const ImageManifest& manifest) {
  if (IsDirectory(source_image)) {
    return ReadFileBytes(source_image / "manifest.json");
  }

  PUU_ASSIGN_OR_RETURN(const std::optional<std::vector<uint8_t>> index,
                       ReadArchiveEntry(source_image, "index.json"));
  if (!index.has_value()) {
    if (manifest.archive_image_name.has_value()) {
      return InvalidArgumentError("{} has no index.json for image '{}'",
                                  source_image.string(),
                                  *manifest.archive_image_name);
    }
    PUU_ASSIGN_OR_RETURN(std::optional<std::vector<uint8_t>> legacy_manifest,
                         ReadArchiveEntry(source_image, "manifest.json"));
    if (legacy_manifest.has_value()) {
      return *std::move(legacy_manifest);
    }
    return InvalidArgumentError("{} has neither index.json nor manifest.json",
                                source_image.string());
  }
  PUU_ASSIGN_OR_RETURN(
      const std::string blob,
      ManifestBlobPath(*index, source_image, manifest.archive_image_name));
  PUU_ASSIGN_OR_RETURN(std::optional<std::vector<uint8_t>> blob_data,
                       ReadArchiveEntry(source_image, blob));
  if (!blob_data.has_value()) {
    return NotFoundError("{} is missing manifest blob {}",
                         source_image.string(), blob);
  }
  return *std::move(blob_data);
}

StatusOr<uint64_t> EstimatedInstallBytes(
    const std::filesystem::path& source_image, const ImageManifest& manifest) {
  if (manifest.install_size.has_value()) {
    return *manifest.install_size;
  }

  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> raw,
                       RawManifest(source_image, manifest));
  PUU_ASSIGN_OR_RETURN(const uint64_t total,
                       LayerTotalBytes(raw, source_image));
  if (total > std::numeric_limits<uint64_t>::max() / kLayerExpansionNumerator) {
    return std::numeric_limits<uint64_t>::max() / kLayerExpansionDenominator;
  }
  return total * kLayerExpansionNumerator / kLayerExpansionDenominator;
}

Status VerifyImageDigest(const std::filesystem::path& source_image,
                         const ImageManifest& manifest,
                         const Digest& expected) {
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> raw,
                       RawManifest(source_image, manifest));
  PUU_ASSIGN_OR_RETURN(const std::string actual, Sha256Hex(raw));
  const std::string_view expected_hex = expected.Hex();
  if (!absl::EqualsIgnoreCase(actual, absl::string_view(expected_hex.data(),
                                                        expected_hex.size()))) {
    return FailedPreconditionError(
        "image manifest digest mismatch for {}: expected sha256:{}, got "
        "sha256:{}",
        source_image.string(), expected.Hex(), actual);
  }
  return absl::OkStatus();
}

}  // namespace puu::engine
