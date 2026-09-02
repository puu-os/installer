#!/usr/bin/env bash

# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026
#
# Builds and tests the installer inside a container. The engine is selected
# with CONTAINER: docker (default), podman, or container (Apple container on
# macOS).

set -euo pipefail

CONTAINER=${CONTAINER:-docker}
CONTAINER_CPUS=${CONTAINER_CPUS:-8}
CONTAINER_MEMORY=${CONTAINER_MEMORY:-16G}

IMAGE=puu-installer-builder:latest

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

run_args=(--rm --volume "${PWD}:/workspace")

# Apple container only shares host directories, needs its services started
# once, and sizes the container at run time; build still configures the
# builder VM the same way.
case "$(basename -- "${CONTAINER}")" in
  container)
    "${CONTAINER}" system status >/dev/null 2>&1 || "${CONTAINER}" system start
    run_args+=(--cpus "${CONTAINER_CPUS}" --memory "${CONTAINER_MEMORY}")
    ;;
esac

"${CONTAINER}" build \
  --cpus "${CONTAINER_CPUS}" \
  --memory "${CONTAINER_MEMORY}" \
  --tag "${IMAGE}" .

# Run as the invoking user so the build trees stay owned by the host user.
"${CONTAINER}" run "${run_args[@]}" \
  --user "$(id -u):$(id -g)" \
  "${IMAGE}" scripts/ci.sh
