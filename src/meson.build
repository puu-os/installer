# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

installer_inc = include_directories('.')

installer_core_sources = files(
  'base/secret_string.cc',
  'base/status.cc',
  'cli.cc',
  'config.cc',
  'engine/block.cc',
  'engine/command.cc',
  'engine/der.cc',
  'engine/firmware.cc',
  'engine/image.cc',
  'engine/partition.cc',
  'engine/passwd.cc',
  'engine/pipeline.cc',
  'engine/progress.cc',
  'engine/runner.cc',
  'engine/secureboot.cc',
  'engine/steps.cc',
  'engine/util.cc',
)

installer_tui_sources = files(
  'tui/app.cc',
  'tui/logging.cc',
  'tui/timezone.cc',
)

version_args = ['-DPUU_INSTALLER_VERSION="' + meson.project_version() + '"']

installer_core = static_library(
  'puu-installer-core',
  installer_core_sources,
  include_directories: installer_inc,
  dependencies: installer_deps,
  cpp_args: version_args,
)

installer_core_dep = declare_dependency(
  link_with: installer_core,
  include_directories: installer_inc,
  dependencies: installer_deps,
)

installer_tui = static_library(
  'puu-installer-tui',
  installer_tui_sources,
  include_directories: installer_inc,
  dependencies: [installer_core_dep] + ftxui_deps,
)

installer_tui_dep = declare_dependency(
  link_with: installer_tui,
  include_directories: installer_inc,
  dependencies: [installer_core_dep] + ftxui_deps,
)

executable(
  'puu-installer',
  files('main.cc'),
  dependencies: [installer_tui_dep],
  cpp_args: version_args,
  install: true,
)

# Assert-based checks over the GPT, Authenticode and SHA-512 crypt arithmetic.
# It includes the .cc files under check, so it must be built with assertions
# on regardless of the buildtype.
selfcheck = executable(
  'puu-selfcheck',
  files('selfcheck.cc'),
  dependencies: [installer_core_dep],
  cpp_args: version_args + ['-UNDEBUG'],
  build_by_default: false,
)

test('selfcheck', selfcheck)
