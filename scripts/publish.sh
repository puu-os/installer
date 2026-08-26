#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026
#
# Publishes a GitLab release consisting of the source tarball and its checksum.
# Run `make package` first.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=sha256.sh
source "$script_dir/sha256.sh"

GLAB="${GLAB:-glab}"

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

for command in git tar xz; do
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

project_version="$(
  sed -n "s/^[[:space:]]*version:[[:space:]]*'\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)'.*/\1/p" \
    meson.build | head -1
)"
[[ "$project_version" == "$version" ]] \
  || die "meson.build version $project_version does not match $version"

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

asset="puu-installer-$version.tar.xz"
asset_path="artifacts/$asset"
asset_checksum="$asset_path.sha256"
[[ -f "$asset_path" && -f "$asset_checksum" ]] \
  || die "missing $asset_path or its checksum; run 'make package' first"

read -r expected_digest _ <"$asset_checksum"
actual_digest="$(sha256_digest "$asset_path")"
[[ "$actual_digest" == "$expected_digest" ]] \
  || die "checksum mismatch for $asset; rerun make package"

# The tarball must correspond to the tag it is published under, otherwise the
# published checksum would attest to a different tree.
# The whole listing is captured first: `tar | head` would SIGPIPE tar under
# pipefail and kill the release for any archive large enough to fill the pipe.
tar_listing="$(tar -tf "$asset_path")" || die "$asset is not a readable tar archive"
read -r first_entry <<<"$tar_listing"
[[ "$first_entry" == "puu-installer-$version/" ]] \
  || die "$asset does not unpack into puu-installer-$version/"

workdir="$(mktemp -d)"
mkdir -p "$workdir/release"
cp "$asset_path" "$asset_checksum" "$workdir/release/"

notes="$workdir/release-notes.md"
git for-each-ref --format='%(contents:subject)%0a%0a%(contents:body)' \
  "refs/tags/$version" >"$notes"

if command -v "$GLAB" >/dev/null 2>&1; then
  if "$GLAB" release view "$version" >/dev/null 2>&1; then
    "$GLAB" release upload "$version" "$workdir"/release/*.tar.xz \
      "$workdir"/release/*.sha256 --clobber
  else
    "$GLAB" release create "$version" "$workdir"/release/*.tar.xz \
      "$workdir"/release/*.sha256 --name "puu-installer $version" \
      --notes-file "$notes"
  fi
else
  printf 'glab not found; copying release assets to artifacts/release\n'
  mkdir -p artifacts/release
  cp "$workdir"/release/*.tar.xz "$workdir"/release/*.sha256 artifacts/release/
fi

printf 'published puu-installer %s\n' "$version"
