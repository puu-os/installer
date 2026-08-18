#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026
#
# Publish a GitLab release from locally packaged static binaries.
# Run `make package` (or PACKAGE_STRICT=1 make package) first.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=sha256.sh
source "$script_dir/sha256.sh"

GLAB="${GLAB:-glab}"
platforms=(linux-aarch64 linux-x86_64)

cleanup() {
  local status=$?

  if [[ -n "${workdir:-}" ]]; then
    rm -rf "$workdir"
  fi
  return "$status"
}
trap cleanup EXIT

die() {
  printf '%s\n' "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

latest_semver_tag() {
  local tag

  while IFS= read -r tag; do
    if [[ "$tag" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      printf '%s\n' "$tag"
      return
    fi
  done < <(git tag --merged HEAD --sort=-v:refname)
  return 1
}

for command in file git tar; do
  require_command "$command"
done
if ! command -v sha256sum >/dev/null 2>&1 && \
  ! command -v shasum >/dev/null 2>&1; then
  die "required command not found: sha256sum or shasum"
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" \
  || die "not inside a Git repository"
cd "$repo_root"

git symbolic-ref --quiet --short HEAD >/dev/null \
  || die "HEAD is detached; check out the release branch before publishing"
[[ -z "$(git status --porcelain)" ]] || die "working directory is not clean"

version="${1:-}"
if [[ -z "$version" ]]; then
  version="$(latest_semver_tag)" || die "no semver tag reachable from HEAD"
fi
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "invalid version: $version"

cargo_version="$(
  sed -n 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)".*/\1/p' \
    Cargo.toml | head -1
)"
[[ "$cargo_version" == "$version" ]] \
  || die "Cargo.toml version $cargo_version does not match $version"

tag_commit="$(git rev-parse "$version^{commit}" 2>/dev/null)" \
  || die "tag $version does not exist"
git verify-tag "$version" >/dev/null \
  || die "tag $version does not have a valid signature"
git merge-base --is-ancestor "$tag_commit" HEAD \
  || die "tag $version is not an ancestor of HEAD"

local_tag="$(git rev-parse "refs/tags/$version")"
remote_output="$(git ls-remote --refs origin "refs/tags/$version")" \
  || die "cannot query tag $version from origin"
remote_tag="${remote_output%%$'\t'*}"
[[ -n "$remote_tag" && "$remote_tag" == "$local_tag" ]] \
  || die "push tag $version to origin before publishing"

workdir="$(mktemp -d)"
mkdir -p "$workdir/release"
missing=()
for platform in "${platforms[@]}"; do
  asset="puu-installer-$version-$platform.tar.gz"
  asset_path="artifacts/$asset"
  asset_checksum="$asset_path.sha256"
  digest_file="artifacts/puu-installer-$version-$platform.bin.sha256"
  if [[ ! -f "$asset_path" || ! -f "$asset_checksum" || ! -f "$digest_file" ]]; then
    missing+=("$platform release asset or checksum")
    continue
  fi

  read -r expected_asset_digest _ <"$asset_checksum"
  actual_asset_digest="$(sha256_digest "$asset_path")"
  [[ "$actual_asset_digest" == "$expected_asset_digest" ]] \
    || die "release asset checksum mismatch for $platform; rerun make package"

  archived_bin="$workdir/$platform-puu-installer"
  tar -xOf "$asset_path" puu-installer >"$archived_bin" \
    || die "release asset for $platform does not contain puu-installer"
  [[ "$(sha256_digest "$archived_bin")" == "$(<"$digest_file")" ]] \
    || die "release asset binary mismatch for $platform; rerun make package"

  description="$(file -b "$archived_bin")"
  case "$platform" in
    linux-aarch64) expected_arch=aarch64 ;;
    linux-x86_64) expected_arch=x86-64 ;;
  esac
  if [[ "${description,,}" != *"$expected_arch"* || \
    "$description" != *"statically linked"* ]]; then
    die "invalid $platform binary: $description"
  fi

  cp "$asset_path" "$asset_checksum" "$workdir/release/"
done

if ((${#missing[@]} > 0)); then
  printf 'missing platform release inputs:\n' >&2
  printf '  %s\n' "${missing[@]}" >&2
  die "run 'PACKAGE_STRICT=1 make package' before publish"
fi

notes="$workdir/release-notes.md"
git for-each-ref --format='%(contents:subject)%0a%0a%(contents:body)' \
  "refs/tags/$version" >"$notes"
if command -v "$GLAB" >/dev/null 2>&1; then
  if "$GLAB" release view "$version" >/dev/null 2>&1; then
    "$GLAB" release upload "$version" "$workdir"/release/*.tar.gz \
      "$workdir"/release/*.sha256 --clobber
  else
    "$GLAB" release create "$version" "$workdir"/release/*.tar.gz \
      "$workdir"/release/*.sha256 --name "puu-installer $version" --notes-file "$notes"
  fi
else
  printf 'glab not found; copying release assets to artifacts/release\n'
  mkdir -p artifacts/release
  cp "$workdir"/release/*.tar.gz "$workdir"/release/*.sha256 artifacts/release/
fi

printf 'published puu-installer %s\n' "$version"
