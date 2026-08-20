<!-- SPDX-License-Identifier: GPL-2.0-or-later -->
<!-- Copyright (C) Opinsys Oy 2026 -->

# puu-installer

<img alt="Puu installing bootc image" src="screenshot.png" width="800">

Standalone installer for bootc-based OSs.

## Configuration

Configuration is provided via `--config-path` or the `PUU_INSTALLER_CONFIG`
environment variable (`--config-path` takes precedence).

The k3s cluster token is passed via `--k3s-token-file`. All other settings are
defined in the configuration file.

Default configuration:

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

## External commands

- **`bootc`**: installs the bootc image to the target filesystem.
- **`mkfs.fat`, `mkfs.xfs`, `mke2fs`**: create target filesystems.
- **`configure-k3s-cluster`**: stages k3s cluster configuration (chrooted).
- **`flatpak-sync`**: pre-seeds system Flatpaks (chrooted).
- **`model-sync`**: pre-seeds vLLM models (chrooted).

## Development

```sh
make ci
```

`make package` cross-compiles static musl binaries with
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) and writes
release tarballs with `.sha256` sidecars under `artifacts/`:

```sh
make package
make package PLATFORMS='linux-x86_64'
PACKAGE_STRICT=1 make package
```

## Release

```sh
scripts/release.sh <version>
git push --atomic origin main <version>
make ci
PACKAGE_STRICT=1 make package
make publish
```

Verify download:

```sh
sha256sum -c puu-installer-<version>-<platform>.tar.gz.sha256
```

## Licensing

GPL-2.0-or-later. Copyright (C) Opinsys Oy 2026.
