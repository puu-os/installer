#!/usr/bin/env bash

# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026
#
# Builds and tests both Linux architectures in containers. CONTAINER selects
# the engine: Apple container on macOS, Docker elsewhere, or an explicit override.

set -euo pipefail

if [[ "$(uname -s)" == Darwin ]]; then
  CONTAINER=${CONTAINER:-container}
else
  CONTAINER=${CONTAINER:-docker}
fi
CONTAINER_CPUS=${CONTAINER_CPUS:-8}
CONTAINER_MEMORY=${CONTAINER_MEMORY:-16G}

if ! command -v "${CONTAINER}" >/dev/null 2>&1; then
  printf 'Install the required container engine: %s\n' "${CONTAINER}" >&2
  exit 1
fi

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

engine=$(basename -- "${CONTAINER}")
build_args=(build)
# The shared mount root can be root-owned even when its files belong to us.
# Trust only this checkout, without changing the host's Git configuration.
run_args=(--rm --volume "${PWD}:/workspace"
  --cpus "${CONTAINER_CPUS}" --memory "${CONTAINER_MEMORY}"
  --user "$(id -u):$(id -g)"
  --env GIT_CONFIG_COUNT=1
  --env GIT_CONFIG_KEY_0=safe.directory
  --env GIT_CONFIG_VALUE_0=/workspace)

if [[ "$engine" == container ]]; then
  "${CONTAINER}" system status >/dev/null 2>&1 || "${CONTAINER}" system start
  build_args+=(--cpus "${CONTAINER_CPUS}" --memory "${CONTAINER_MEMORY}")
fi

status=0
for arch in arm64 amd64; do
  platform="linux/$arch"
  image="puu-installer-builder:$arch"
  platform_args=(--platform "$platform")
  if [[ "$engine" == container && "$arch" == amd64 ]]; then
    platform_args+=(--rosetta)
  fi

  printf '==> build CI image for %s\n' "$platform"
  if ! "${CONTAINER}" "${build_args[@]}" --platform "$platform" --tag "$image" .; then
    printf 'CI image build failed for %s\n' "$platform" >&2
    status=1
    continue
  fi

  printf '==> run CI for %s\n' "$platform"
  # Separate build trees prevent caches and binaries crossing architectures.
  if ! "${CONTAINER}" run "${run_args[@]}" "${platform_args[@]}" \
    --env "BUILDDIR=build/$arch" \
    --env "SANITIZE_BUILDDIR=build-sanitize/$arch" \
    "$image" scripts/container-ci.sh; then
    printf 'CI failed for %s\n' "$platform" >&2
    status=1
  fi
done

exit "$status"
