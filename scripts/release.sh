#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Copyright (C) Jarkko Sakkinen 2026

set -euo pipefail

die() {
	echo "$1" >&2
	exit 1
}

ver_gt() {
	if   (( $1 > $4 )); then return 0
	elif (( $1 == $4 && $2 > $5 )); then return 0
	elif (( $1 == $4 && $2 == $5 && $3 > $6 )); then return 0
	else return 1
	fi
}

committed=0

cleanup() {
	local status=$?
	if (( status != 0 && !committed )); then
		git restore -- Cargo.toml Cargo.lock 2>/dev/null || true
	fi
	return "$status"
}
trap cleanup EXIT

next_ver="${1:-}"
[[ -n "$next_ver" ]] || die "usage: scripts/release.sh <next-version>"

[[ "$next_ver" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] \
	|| die "invalid version: $next_ver"
next_a="${BASH_REMATCH[1]}"
next_b="${BASH_REMATCH[2]}"
next_c="${BASH_REMATCH[3]}"

branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null)" \
	|| die "HEAD is detached; check out a branch before releasing"

[[ -z "$(git status --porcelain)" ]] \
	|| die "working directory is not clean"

[[ -z "$(git tag -l "$next_ver")" ]] \
	|| die "tag $next_ver already exists"

pkg="$(sed -n 's/^[[:space:]]*name[[:space:]]*=[[:space:]]*"\([^"]\+\)".*/\1/p' Cargo.toml | head -1)"
[[ -n "$pkg" ]] || die "cannot find package name in Cargo.toml"

cur_ver="$(sed -n 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)".*/\1/p' Cargo.toml | head -1)"
[[ -n "$cur_ver" ]] || die "cannot find version in Cargo.toml"

[[ "$cur_ver" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] \
	|| die "cannot parse version components from: $cur_ver"
cur_a="${BASH_REMATCH[1]}"
cur_b="${BASH_REMATCH[2]}"
cur_c="${BASH_REMATCH[3]}"

ver_gt "$next_a" "$next_b" "$next_c" "$cur_a" "$cur_b" "$cur_c" \
	|| die "$next_ver is not greater than current $cur_ver"

sed -i -E "0,/(^[[:space:]]*version[[:space:]]*=[[:space:]]*\")${cur_ver//./\\.}(\")/s//\1$next_ver\2/" Cargo.toml
grep -q "^version = \"$next_ver\"" Cargo.toml \
	|| die "failed to update version in Cargo.toml"

cargo metadata --format-version 1 >/dev/null
grep -A2 "^name = \"$pkg\"" Cargo.lock | grep -q "^version = \"$next_ver\"" \
	|| die "failed to update version in Cargo.lock"

cargo clippy --all-targets --locked

git rev-parse -q --verify "refs/tags/$cur_ver" >/dev/null \
	|| die "current version tag $cur_ver does not exist"
range="${cur_ver}..HEAD"

log=""
while IFS=$'\x1f' read -r subj author; do
	log+="- $subj ($author)"$'\n'
done < <(git log --pretty=tformat:'%s%x1f%an' --no-merges "$range")
log="${log%$'\n'}"

git commit -a -s -m "Bump the version to $next_ver"
committed=1

sob="Signed-off-by: $(git config user.name) <$(git config user.email)>"
printf '%s %s\n\n%s\n\n%s\n' "$pkg" "$next_ver" "$log" "$sob" | git tag -s "$next_ver" -F -

echo "tagged $next_ver"
