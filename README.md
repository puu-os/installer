<!-- SPDX-License-Identifier: GPL-2.0-or-later -->
<!-- Copyright (C) Opinsys Oy 2026 -->

# Puu OS installer

Puu OS installer is a standalone installer for bootc based operating systems.

## Deployment configuration

A deployment configuration must be provided either via `--config-path` or
`PUU_INSTALLER_CONFIG`.

This is the default configuration:

```yaml
branding:
  name: Puu
partitions:
  efi: 512M
  rootMin: 64G
  home: 256G
labels:
  root: puu-root
  home: puu-home
image:
  targetImgRef: quay.io/puu/puu:latest
  expectedDigest: ""       # sha256 of raw OCI manifest for local sources
  sourceLabel: PAYLOAD
  sourceMount: /run/puu-installer/payload
identity:
  keymap: us
  locale: C.UTF-8
  timezone: UTC
  hostname: puu
flatpak:
  manifestPath: /usr/share/puu/flatpaks.list
  syncHelper: /usr/libexec/puu/flatpak-sync
  liveStore: /var/lib/flatpak
models:
  manifestPath: /usr/share/puu/models.list
  syncHelper: /usr/libexec/puu/model-sync
  liveStore: /var/lib/vllm/models
k3s:
  helper: /usr/libexec/puu/configure-k3s-cluster
  role: server          # "server" or "agent"
  serverUrl: ""         # required when role is "agent"
  nodeName: ""
homed:
  groups:
    - adm
    - audio
    - cdrom
    - dip
    - input
    - lpadmin
    - netdev
    - plugdev
    - render
    - sudo
    - video
    - wheel
    - realtime
    - uucp
serialConsole: {}       # map of machine name -> console
```
Optional K3s token must be passed via `--k3s-token-file`.

## Development

The installer targets Linux only. On both macOS and Linux, `make ci` builds
and tests Linux arm64 and amd64 in containers, including sanitizer checks.
`scripts/release.sh <next-version>` runs the same CI before committing and tagging.
Git signing and `make publish` stay on the host; publishing requires `glab`.

Install Apple `container` with Rosetta support on macOS, or Docker with
cross-architecture emulation on Linux. Set `CONTAINER=podman` or
`CONTAINER=docker` to override the engine; it must support both architectures.
`CONTAINER_CPUS` and `CONTAINER_MEMORY` default to 8 and 16G per container.
Build trees are isolated under `build/{arm64,amd64}` and
`build-sanitize/{arm64,amd64}`. No native macOS build dependencies are needed.

For native development on Linux (`make all`, `make test`, or `make tidy`):

- Install the build dependencies listed in `Dockerfile`.
- Install `abseil-cpp` for foundational C++ utilities:
  - Debian/Ubuntu: `libabsl-dev`.
  - Fedora: `abseil-cpp-devel`.
- Follow [Google C++ Style Guide](https://google.github.io/styleguide/cppguide.html).
- Vendor dependencies other than Abseil and the existing platform libraries with
  [FetchContent](https://cmake.org/cmake/help/latest/module/FetchContent.html),
  pinned to hash-checked upstream release tarballs.

## Licensing

Puu OS installer has been licensed with the GNU General Public License 2.0, or a
newer version. Study [`LICENSE`](LICENSE) for further details.
