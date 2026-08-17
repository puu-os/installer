#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026
#
# Run cargo against the Linux musl release target when the host is not
# Linux. On Linux, run cargo as given. Source this file or execute it.

linux_musl_target=x86_64-unknown-linux-musl
linux_musl_zig_target=x86_64-linux-musl

if ! declare -F die >/dev/null; then
  die() {
    printf '%s\n' "$1" >&2
    exit 1
  }
fi

# Zig's linker opens every codegen unit at once. macOS login shells often
# inherit launchd's 256-file soft limit, which is not enough for a debug
# musl crate. Raise the soft limit toward the hard cap when we can.
raise_nofile_limit() {
  local want=10240
  local soft hard

  soft="$(ulimit -Sn)"
  hard="$(ulimit -Hn)"
  if [[ "$soft" == unlimited ]]; then
    return 0
  fi
  if ((soft >= want)); then
    return 0
  fi
  if [[ "$hard" == unlimited ]] || ((hard >= want)); then
    ulimit -Sn "$want" 2>/dev/null && return 0
  fi
  if [[ "$hard" != unlimited ]]; then
    ulimit -Sn "$hard" 2>/dev/null && return 0
  fi
  ulimit -Sn 4096 2>/dev/null && return 0
  ulimit -Sn 1024 2>/dev/null || true
}

linux_cargo_with_target() {
  local args=("$@")
  local has_target=0
  local inserted=0
  local arg
  local out=()

  for arg in "${args[@]}"; do
    if [[ "$arg" == --target || "$arg" == --target=* ]]; then
      has_target=1
    fi
  done
  if ((has_target == 0)); then
    for arg in "${args[@]}"; do
      if [[ "$arg" == -- && inserted -eq 0 ]]; then
        out+=(--target "$linux_musl_target")
        inserted=1
      fi
      out+=("$arg")
    done
    if ((inserted == 0)); then
      out+=(--target "$linux_musl_target")
    fi
    args=("${out[@]}")
  fi
  printf '%s\0' "${args[@]}"
}

linux_cargo() {
  local cargo="${CARGO:-cargo}"
  local zigbuild="${CARGO_ZIGBUILD:-cargo-zigbuild}"
  local args=("$@")
  local zigcc=""
  local zigbuild_path
  local status=0
  local cmd
  local -a rest

  if [[ "$(uname -s)" == Linux ]]; then
    "$cargo" "${args[@]}"
    return
  fi

  command -v "$zigbuild" >/dev/null 2>&1 \
    || die "$zigbuild is required to build on $(uname -s)"
  command -v rustup >/dev/null 2>&1 \
    || die "rustup is required to build on $(uname -s)"
  rustup target list --installed | grep -Fxq "$linux_musl_target" \
    || die "rustup target $linux_musl_target is not installed"

  raise_nofile_limit
  export CARGO_INCREMENTAL=0
  export CARGO_PROFILE_DEV_CODEGEN_UNITS="${CARGO_PROFILE_DEV_CODEGEN_UNITS:-1}"

  local -a with_target=()
  while IFS= read -r -d '' arg; do
    with_target+=("$arg")
  done < <(linux_cargo_with_target "${args[@]}")
  args=("${with_target[@]}")

  cmd="${args[0]}"
  rest=("${args[@]:1}")

  case "$cmd" in
    build|test|run|rustc|bench)
      "$zigbuild" zigbuild "${rest[@]}"
      return
      ;;
  esac

  zigbuild_path="$(command -v "$zigbuild")"
  zigcc="$(mktemp "${TMPDIR:-/tmp}/puu-zigcc.XXXXXX")"
  printf '%s\n' '#!/bin/sh' \
    "exec $(printf '%q' "$zigbuild_path") zig cc -- -target $linux_musl_zig_target \"\$@\"" \
    >"$zigcc"
  chmod +x "$zigcc"

  CC_x86_64_unknown_linux_musl="$zigcc" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$zigcc" \
    "$cargo" "${args[@]}" || status=$?
  rm -f "$zigcc"
  return "$status"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  set -euo pipefail
  linux_cargo "$@"
fi
