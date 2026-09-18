# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

# Directory scope keeps the warnings and sanitizers off the vendored
# dependencies.
if(PUU_SANITIZE)
  add_compile_options(-fsanitize=${PUU_SANITIZE})
  add_link_options(-fsanitize=${PUU_SANITIZE})
endif()
add_compile_options(${PUU_COMPILE_OPTIONS})
add_compile_definitions(PUU_INSTALLER_VERSION="${PROJECT_VERSION}")

set(installer_core_sources
  base/secret_string.cc
  base/status.cc
  cli.cc
  config.cc
  engine/block.cc
  engine/command.cc
  engine/der.cc
  engine/firmware.cc
  engine/image.cc
  engine/partition.cc
  engine/passwd.cc
  engine/pipeline.cc
  engine/progress.cc
  engine/runner.cc
  engine/secureboot.cc
  engine/steps.cc
  engine/util.cc)

set(installer_tui_sources
  tui/app.cc
  tui/logging.cc
  tui/timezone.cc)

add_library(puu-installer-core STATIC ${installer_core_sources})
target_include_directories(puu-installer-core PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(puu-installer-core PUBLIC
  argtable3::argtable3
  yaml-cpp::yaml-cpp
  fmt::fmt
  spdlog::spdlog
  nlohmann_json::nlohmann_json
  absl::base
  absl::status
  absl::statusor
  absl::strings
  absl::synchronization
  absl::time
  OpenSSL::SSL
  OpenSSL::Crypto
  PkgConfig::BLKID
  PkgConfig::UDEV
  PkgConfig::MOUNT
  Threads::Threads)

add_executable(puu-installer main.cc ${installer_tui_sources})
target_link_libraries(puu-installer PRIVATE puu-installer-core ftxui::component)
install(TARGETS puu-installer RUNTIME)

# Assert-based checks over the GPT, Authenticode and SHA-512 crypt arithmetic.
# Keep assertions enabled regardless of build type: -UNDEBUG lands after the
# per-config -DNDEBUG on the compiler command line.
add_executable(puu-selfcheck selfcheck.cc)
target_compile_options(puu-selfcheck PRIVATE -UNDEBUG)
target_link_libraries(puu-selfcheck PRIVATE puu-installer-core)
add_test(NAME selfcheck COMMAND puu-selfcheck)
