#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026
#
# Builds the source release: a .tar.xz of the committed tree plus a .sha256
# sidecar, written to $BUILDDIR/artifacts/.
#
# The installer is no longer shipped as a pre-compiled binary. `meson dist`
# archives HEAD, then unpacks, builds and tests the archive, so a tarball that
# does not build cannot be released.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=sha256.sh
source "$script_dir/sha256.sh"

MESON="${MESON:-meson}"
BUILDDIR="${BUILDDIR:-build}"

die() {
  printf '%s\n' "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

for command in "$MESON" git tar xz; do
  require_command "$command"
done
if ! command -v sha256sum >/dev/null 2>&1 && \
  ! command -v shasum >/dev/null 2>&1; then
  die "required command not found: sha256sum or shasum"
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" \
  || die "not inside a Git repository"
cd "$repo_root"

# meson dist archives HEAD, so anything uncommitted would silently be left out.
[[ -z "$(git status --porcelain)" ]] \
  || die "working directory is not clean; commit before packaging"

[[ -d "$BUILDDIR" ]] || "$MESON" setup "$BUILDDIR"

# Read the version from meson.build rather than from introspection, whose
# output also carries every subproject's version.
version="$(
  sed -n "s/^[[:space:]]*version:[[:space:]]*'\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)'.*/\1/p" \
    meson.build | head -1
)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || die "cannot determine project version from meson.build"

# Subprojects are deliberately not bundled: meson dist would archive the
# upstream tarballs without their WrapDB patches, which produces a tree that
# cannot configure. The wrap files travel with the release and Meson fetches
# the pinned, hash-checked sources at build time.
printf '==> building source release %s\n' "$version"
"$MESON" dist -C "$BUILDDIR" --formats xztar

asset="puu-installer-$version.tar.xz"
dist_path="$BUILDDIR/meson-dist/$asset"
[[ -f "$dist_path" ]] || die "meson dist did not produce $dist_path"

artifacts_dir="$BUILDDIR/artifacts"
mkdir -p "$artifacts_dir"
cp "$dist_path" "$artifacts_dir/$asset"
write_sha256_sidecar "$artifacts_dir/$asset"

printf 'packaged %s/%s\n' "$artifacts_dir" "$asset"
printf 'checksum %s/%s.sha256\n' "$artifacts_dir" "$asset"
