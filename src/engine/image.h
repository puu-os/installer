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
};

// Locates the local bootc source image.
//
// Prefers an already-mounted payload, otherwise finds the filesystem labelled
// `image.sourceLabel` and mounts it read-only. Returns nullopt when running in
// a container, where the image is pulled from the network instead.
StatusOr<std::optional<std::filesystem::path>> DefaultSourceImage(
    const Config& config);

StatusOr<ImageManifest> LoadImageManifest(
    const std::filesystem::path& source_image);

// Resolves the image reference to install: the sidecar manifest wins over the
// configured `image.targetImgRef`.
StatusOr<ImageRef> LoadTargetImgRef(const ImageManifest& manifest,
                                    const Config& config);

// Returns the raw bytes of the OCI manifest inside `source_image`.
//
// The digest is taken over these exact bytes, not over the archive, so the
// comparison matches what a registry would report.
StatusOr<std::vector<uint8_t>> RawManifest(
    const std::filesystem::path& source_image, const ImageManifest& manifest);

// Estimated number of bytes the install will write, used to drive the progress
// bar for `bootc install`.
StatusOr<uint64_t> EstimatedInstallBytes(
    const std::filesystem::path& source_image, const ImageManifest& manifest);

Status VerifyImageDigest(const std::filesystem::path& source_image,
                         const ImageManifest& manifest, const Digest& expected);

}  // namespace puu::engine

#endif  // PUU_ENGINE_IMAGE_H_
