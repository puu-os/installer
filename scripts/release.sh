#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

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
signing_check_tag=""

cleanup() {
	local status=$?

	if [[ -n "$signing_check_tag" ]] \
		&& git rev-parse --verify --quiet \
			"refs/tags/$signing_check_tag" >/dev/null; then
		git tag -d "$signing_check_tag" >/dev/null 2>&1 || true
	fi
	rm -f Cargo.toml.next
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

signing_check_tag="puu-installer-signing-check-$next_ver-$$"
[[ -z "$(git tag -l "$signing_check_tag")" ]] \
	|| die "temporary signing-check tag already exists: $signing_check_tag"
git tag -s "$signing_check_tag" -m "puu-installer release signing check" \
	|| die "cannot sign release tags; renew or configure the Git signing key"
git tag -d "$signing_check_tag" >/dev/null
signing_check_tag=""

toml_string() {
	awk -v key="$1" '
		$0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
			if (match($0, /"[^"]+"/)) {
				print substr($0, RSTART + 1, RLENGTH - 2)
				exit
			}
		}
	' Cargo.toml
}

pkg="$(toml_string name)"
[[ -n "$pkg" ]] || die "cannot find package name in Cargo.toml"

cur_ver="$(toml_string version)"
[[ -n "$cur_ver" ]] || die "cannot find version in Cargo.toml"

[[ "$cur_ver" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] \
	|| die "cannot parse version components from: $cur_ver"
cur_a="${BASH_REMATCH[1]}"
cur_b="${BASH_REMATCH[2]}"
cur_c="${BASH_REMATCH[3]}"

ver_gt "$next_a" "$next_b" "$next_c" "$cur_a" "$cur_b" "$cur_c" \
	|| die "$next_ver is not greater than current $cur_ver"

awk -v cur="$cur_ver" -v nxt="$next_ver" '
	!done && $0 ~ /^[[:space:]]*version[[:space:]]*=[[:space:]]*"/ {
		sub("\"" cur "\"", "\"" nxt "\"")
		done = 1
	}
	{ print }
' Cargo.toml > Cargo.toml.next
mv Cargo.toml.next Cargo.toml
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

printf 'tagged %s\n' "$next_ver"
printf 'push the commit and tag: git push --atomic origin %q %q\n' \
	"HEAD:refs/heads/$branch" "refs/tags/$next_ver"
printf 'then run: make ci && PACKAGE_STRICT=1 make package && make publish\n'
