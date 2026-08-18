#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026
#
# Multi-platform release packaging.
#   host-native triple — cargo
#   other triples — cargo-zigbuild (Zig linker)
#
# Platforms:
#   linux-x86_64, linux-aarch64 — x86_64/aarch64-unknown-linux-musl (static)
#
# Exit 0 if every requested platform was packaged.
# Exit 2 if a requested build fails while another succeeds.
# Exit 1 if nothing was packaged or a hard prerequisite is missing.
# PACKAGE_STRICT=1 fails when any requested platform is skipped or failed.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=package-target.sh
source "$script_dir/package-target.sh"
# shellcheck source=sha256.sh
source "$script_dir/sha256.sh"
# shellcheck source=linux-cargo.sh
source "$script_dir/linux-cargo.sh"

CARGO="${CARGO:-cargo}"
RUSTC="${RUSTC:-rustc}"
CARGO_ZIGBUILD="${CARGO_ZIGBUILD:-cargo-zigbuild}"
FILE="${FILE:-file}"
READELF="${READELF:-readelf}"
ZIG="${ZIG:-zig}"

export PATH="${HOME}/.cargo/bin:${PATH}"

die() {
  printf '%s\n' "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

for command in "$CARGO" "$RUSTC" git tar "$FILE"; do
  require_command "$command"
done
if ! command -v sha256sum >/dev/null 2>&1 && \
  ! command -v shasum >/dev/null 2>&1; then
  die "required command not found: sha256sum or shasum"
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" \
  || die "not inside a Git repository"
cd "$repo_root"

build_root=

cleanup_package() {
  local status=$?
  if [[ -n "${build_root:-}" ]]; then
    rm -rf "$build_root"
  fi
  return "$status"
}
trap cleanup_package EXIT

version="$(
  sed -n 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)".*/\1/p' \
    Cargo.toml | head -1
)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "invalid Cargo.toml version: $version"

build_root="${XDG_CACHE_HOME:-$HOME/.cache}/puu-installer/package-src/$version-$$"
mkdir -p "$build_root"
git archive HEAD | tar -x -C "$build_root"
printf 'packaging HEAD (%s) from an isolated source tree\n' "$version"
[[ -f "$build_root/Cargo.toml" ]] || die "HEAD does not contain Cargo.toml"
archived_version="$(
  sed -n 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)".*/\1/p' \
    "$build_root/Cargo.toml" | head -1
)"
[[ "$archived_version" == "$version" ]] \
  || die "archived Cargo.toml version $archived_version does not match $version"

rust_host_triple="$("$RUSTC" --print host-tuple)"
[[ -n "$rust_host_triple" ]] || die "cannot determine rustc host triple"

package_target_root="${CARGO_TARGET_DIR:-$repo_root/target/package}"

# platform|rust-triple|binary-name
platforms=(
  'linux-x86_64|x86_64-unknown-linux-musl|puu-installer'
  'linux-aarch64|aarch64-unknown-linux-musl|puu-installer'
)

requested=()
if (($# > 0)); then
  requested=("$@")
else
  for entry in "${platforms[@]}"; do
    IFS='|' read -r platform _ _ <<<"$entry"
    requested+=("$platform")
  done
fi

platform_entry() {
  local want="$1" entry platform
  for entry in "${platforms[@]}"; do
    IFS='|' read -r platform _ _ <<<"$entry"
    if [[ "$platform" == "$want" ]]; then
      printf '%s\n' "$entry"
      return 0
    fi
  done
  return 1
}

verify_linux_static() {
  local bin="$1"
  local description

  description="$($FILE -b "$bin")"
  printf '%s\n' "$description"
  if [[ "$description" != *"static"* ]]; then
    printf 'Linux musl release is not statically linked: %s\n' "$bin" >&2
    return 1
  fi
  if command -v "$READELF" >/dev/null 2>&1; then
    if "$READELF" -l "$bin" 2>/dev/null | grep -q 'INTERP'; then
      printf 'Linux musl release declares a dynamic ELF interpreter: %s\n' "$bin" >&2
      return 1
    fi
    if "$READELF" -d "$bin" 2>/dev/null | grep -q '(NEEDED)'; then
      printf 'Linux musl release has dynamic dependencies: %s\n' "$bin" >&2
      return 1
    fi
  fi
  return 0
}

rustup_has_target() {
  local triple="$1"
  command -v rustup >/dev/null 2>&1 || return 0
  rustup target list --installed | grep -Fxq "$triple"
}

# Host ~/.cargo/config.toml (mold/clang for all Linux) sets -C linker and
# opts cargo-zigbuild out of zig cc. Use a dedicated CARGO_HOME with no config.
package_cargo_home() {
  local cargo_home="${XDG_CACHE_HOME:-$HOME/.cache}/puu-installer/package-cargo"
  mkdir -p "$cargo_home"
  rm -f "$cargo_home/config.toml" "$cargo_home/config"
  printf '%s\n' "$cargo_home"
}

build_with_zigbuild() {
  local triple="$1"
  local target_dir="$package_target_root/$triple"
  local cargo_home zig_path
  cargo_home="$(package_cargo_home)"
  zig_path="$(command -v "$ZIG")"

  (
    cd "$build_root" || exit 1
    export CARGO_HOME="$cargo_home"
    export CARGO_TARGET_DIR="$target_dir"
    export CARGO_ZIGBUILD_ZIG_PATH="$zig_path"
    export CARGO_INCREMENTAL=0
    unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
    raise_nofile_limit
    "$CARGO_ZIGBUILD" zigbuild --release --target "$triple"
  )
}

build_native() {
  local triple="$1"
  local target_dir="$package_target_root/$triple"

  (
    cd "$build_root" || exit 1
    CARGO_TARGET_DIR="$target_dir" "$CARGO" build --release --target "$triple"
  )
}

needs_zigbuild=0
for platform in "${requested[@]}"; do
  entry="$(platform_entry "$platform")" \
    || die "unknown platform: $platform (supported: linux-x86_64 linux-aarch64)"
  IFS='|' read -r _ triple _ <<<"$entry"
  if [[ "$(package_build_driver "$rust_host_triple" "$triple")" == zigbuild ]]; then
    needs_zigbuild=1
  fi
done
if ((needs_zigbuild)); then
  require_command "$CARGO_ZIGBUILD"
  require_command "$ZIG"
fi

built=()
failed=()

mkdir -p artifacts

for platform in "${requested[@]}"; do
  entry="$(platform_entry "$platform")" \
    || die "unknown platform: $platform (supported: linux-x86_64 linux-aarch64)"

  IFS='|' read -r _ triple binary <<<"$entry"

  if [[ "$(package_build_driver "$rust_host_triple" "$triple")" == cargo ]]; then
    printf '==> %s (%s via cargo)\n' "$platform" "$triple"
    if ! build_native "$triple"; then
      failed+=("$platform: cargo build failed")
      printf 'fail: %s (cargo build failed)\n' "$platform" >&2
      continue
    fi
  else
    printf '==> %s (%s via cargo-zigbuild)\n' "$platform" "$triple"
    if ! rustup_has_target "$triple"; then
      failed+=("$platform: rustup target not installed: $triple")
      printf 'fail: %s (rustup target add %s)\n' "$platform" "$triple" >&2
      continue
    fi

    if ! build_with_zigbuild "$triple"; then
      failed+=("$platform: cargo zigbuild failed")
      printf 'fail: %s (cargo zigbuild failed)\n' "$platform" >&2
      continue
    fi
  fi

  source_bin="$package_target_root/$triple/$triple/release/$binary"
  if [[ ! -f "$source_bin" ]]; then
    failed+=("$platform: missing $source_bin")
    printf 'fail: %s (missing %s)\n' "$platform" "$source_bin" >&2
    continue
  fi

  if ! verify_linux_static "$source_bin"; then
    failed+=("$platform: not statically linked")
    continue
  fi

  asset="puu-installer-$version-$platform.tar.gz"
  tar -C "$(dirname "$source_bin")" -czf "artifacts/$asset" "$binary"
  printf '%s\n' "$(sha256_digest "$source_bin")" \
    >"artifacts/puu-installer-$version-$platform.bin.sha256"
  write_sha256_sidecar "artifacts/$asset"
  built+=("$platform")
  printf 'packaged %s -> artifacts/%s\n' "$platform" "$asset"
done

printf '\n'
if ((${#built[@]})); then
  printf 'built: %s\n' "${built[*]}"
else
  printf 'built: (none)\n'
fi
if ((${#failed[@]})); then
  printf 'failed:\n' >&2
  printf '  %s\n' "${failed[@]}" >&2
fi

if ((${#built[@]} == 0)); then
  die "no platforms were packaged"
fi

if [[ "${PACKAGE_STRICT:-0}" == 1 ]]; then
  if ((${#failed[@]} > 0)); then
    die "PACKAGE_STRICT=1 and some platforms failed"
  fi
fi

if ((${#failed[@]} > 0)); then
  exit 2
fi
