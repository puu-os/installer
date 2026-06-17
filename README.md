<!-- SPDX-License-Identifier: GPL-2.0-or-later -->
<!-- Copyright (C) Opinsys Oy 2026 -->

# puu-installer

Standalone installer for bootc-based OSs.

## Configuration

The installer reads a YAML configuration file. Its path is taken from
`--config`, or from the `PUU_INSTALLER_CONFIG` environment variable when
`--config` is not given. `--config` takes precedence over the environment
variable.

The k3s cluster token is the only k3s setting passed on the command line, via
`--k3s-token-file` (the file's first line is used). All other k3s settings live
in the configuration file.

All keys are optional; the values below are the defaults.

```yaml
partitions:
  efi: 512M
  rootMin: 64G
  home: 256G
labels:
  root: root
  home: home
image:
  targetImgRef: quay.io/puu/puu:latest
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
kernel:
  extraArm64Args:
    - cma=256M
```

## Licensing

`puu-installer` is licensed under the `GPL-2.0-or-later` license.

Copyright (C) Opinsys Oy 2026
