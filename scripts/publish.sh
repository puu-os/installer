#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026
#
# Publishes a GitLab release from the tag and its annotations.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source-path=SCRIPTDIR
# shellcheck source=project-version.sh
source "$script_dir/project-version.sh"

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

require_command git
require_command "$GLAB"

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

project_version="$(read_project_version CMakeLists.txt)"
[[ "$project_version" == "$version" ]] \
  || die "CMakeLists.txt version $project_version does not match $version"

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
notes="$workdir/release-notes.md"
git for-each-ref --format='%(contents:subject)%0a%0a%(contents:body)' \
  "refs/tags/$version" >"$notes"

if "$GLAB" release view "$version" >/dev/null 2>&1; then
  die "release $version already exists"
fi

"$GLAB" release create "$version" --name "puu-installer $version" \
  --notes-file "$notes"

printf 'published puu-installer %s\n' "$version"
