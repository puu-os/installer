// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_IMAGE_H_
#define PUU_ENGINE_IMAGE_H_

#include <cstdint>
#include <filesystem>
#include <optional>
#include <string>
#include <vector>

#include "base/status.h"
#include "config.h"

namespace puu::engine {

// Optional metadata shipped next to a local image as "image.manifest".
struct ImageManifest {
  std::optional<ImageRef> target_imgref;
  std::optional<std::string> version;
  // Installed size in bytes, used to estimate installation progress.
  std::optional<uint64_t> install_size;
  std::optional<ImageRef> source_imgref;
  // OCI index annotation selected by the sidecar's imgref field.
  std::optional<std::string> archive_image_name;

  static StatusOr<ImageManifest> Load(
      const std::filesystem::path& source_image);

  // Resolves the image reference to install: the sidecar manifest wins over the
  // configured `image.targetImgRef`.
  [[nodiscard]] StatusOr<ImageRef> TargetImgRef(const Config& config) const;

  // Estimated number of bytes the install will write, used to drive the
  // progress bar for `bootc install`.
  [[nodiscard]] StatusOr<uint64_t> EstimatedInstallBytes(
      const std::filesystem::path& source_image) const;

  [[nodiscard]] Status VerifyDigest(const std::filesystem::path& source_image,
                                    const Digest& expected) const;
};

// Locates the local bootc source image.
//
// Prefers an already-mounted payload, otherwise finds the filesystem labelled
// `image.sourceLabel` and mounts it read-only. Returns nullopt when running in
// a container, where the image is pulled from the network instead.
StatusOr<std::optional<std::filesystem::path>> DefaultSourceImage(
    const Config& config);

}  // namespace puu::engine

#endif  // PUU_ENGINE_IMAGE_H_
