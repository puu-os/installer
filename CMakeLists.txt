# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

cmake_minimum_required(VERSION 3.31)

project(
  puu-installer
  VERSION 1.0.53
  LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
# No C++ modules are used; scanning would also inject GCC module flags into
# the compile database, which clang-tidy cannot parse.
set(CMAKE_CXX_SCAN_FOR_MODULES OFF)

if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Debug CACHE STRING "Build type" FORCE)
endif()

set(CMAKE_POSITION_INDEPENDENT_CODE ON)

# Sanitizers are applied to project code only (see src/CMakeLists.txt).
# Dependencies remain uninstrumented, which also sidesteps GCC 14 rejecting
# Abseil's hash-table internals under -fsanitize=undefined.
set(PUU_SANITIZE ""
    CACHE STRING "Comma-separated -fsanitize= list applied to project code")

set(PUU_COMPILE_OPTIONS "")
if(CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang")
  list(APPEND PUU_COMPILE_OPTIONS
    -Wall
    -Wextra
    -Wpedantic
    -Wconversion
    -Wsign-conversion
    -Wshadow
    -Wnon-virtual-dtor
    -Wold-style-cast
    -Woverloaded-virtual
    -Wdouble-promotion
    -Wimplicit-fallthrough
    -Wno-missing-field-initializers
    -Wformat=2)
endif()

include(CheckCXXCompilerFlag)
check_cxx_compiler_flag(-fstack-protector-strong PUU_HAS_STACK_PROTECTOR_STRONG)
if(PUU_HAS_STACK_PROTECTOR_STRONG)
  list(APPEND PUU_COMPILE_OPTIONS -fstack-protector-strong)
endif()

# Vendored dependencies, pinned to the same hash-checked upstream tarballs the
# former Meson wraps used. They are private implementation details: always
# built from source, static, SYSTEM includes, and excluded from the install.
include(FetchContent)

set(FMT_INSTALL OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  fmt
  URL https://github.com/fmtlib/fmt/archive/12.0.0.tar.gz
  URL_HASH SHA256=aa3e8fbb6a0066c03454434add1f1fc23299e85758ceec0d7d2d974431481e40
  SYSTEM
  EXCLUDE_FROM_ALL)

set(SPDLOG_FMT_EXTERNAL ON CACHE BOOL "" FORCE)
set(SPDLOG_INSTALL OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  spdlog
  URL https://github.com/gabime/spdlog/archive/refs/tags/v1.17.0.tar.gz
  URL_HASH SHA256=d8862955c6d74e5846b3f580b1605d2428b11d97a410d86e2fb13e857cd3a744
  SYSTEM
  EXCLUDE_FROM_ALL)

set(ARGTABLE3_ENABLE_TESTS OFF CACHE BOOL "" FORCE)
set(ARGTABLE3_ENABLE_EXAMPLES OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  argtable3
  URL https://github.com/argtable/argtable3/releases/download/v3.3.1/argtable-v3.3.1.tar.gz
  URL_HASH SHA256=c08bca4b88ddb9234726b75455b3b1670d7c864d8daf198eaa7a3b4d41addf2c
  SYSTEM
  EXCLUDE_FROM_ALL)

set(YAML_CPP_BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(YAML_CPP_BUILD_TOOLS OFF CACHE BOOL "" FORCE)
set(YAML_CPP_INSTALL OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  yaml-cpp
  URL https://github.com/jbeder/yaml-cpp/archive/refs/tags/0.8.0.zip
  URL_HASH SHA256=334e80ab7b52e14c23f94e041c74bab0742f2281aad55f66be2f19f4b7747071
  SYSTEM
  EXCLUDE_FROM_ALL)

set(JSON_BuildTests OFF CACHE BOOL "" FORCE)
set(JSON_Install OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  nlohmann_json
  URL https://github.com/nlohmann/json/releases/download/v3.12.0/json.tar.xz
  URL_HASH SHA256=42f6e95cad6ec532fd372391373363b62a14af6d771056dbfc86160e6dfff7aa
  SYSTEM
  EXCLUDE_FROM_ALL)

set(FTXUI_BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
set(FTXUI_ENABLE_INSTALL OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  ftxui
  URL https://github.com/ArthurSonzogni/FTXUI/archive/refs/tags/v6.1.9.tar.gz
  URL_HASH SHA256=45819c1e54914783d4a1ca5633885035d74146778a1f74e1213cdb7b76340e71
  SYSTEM
  EXCLUDE_FROM_ALL)

# fmt must precede spdlog so spdlog picks up the fmt::fmt target instead of
# calling find_package(fmt).
FetchContent_MakeAvailable(
  fmt
  spdlog
  argtable3
  yaml-cpp
  nlohmann_json
  ftxui)

find_package(absl REQUIRED)
find_package(OpenSSL 3.0 REQUIRED)
find_package(Threads REQUIRED)
find_package(PkgConfig REQUIRED)
pkg_check_modules(BLKID REQUIRED IMPORTED_TARGET blkid)
pkg_check_modules(UDEV REQUIRED IMPORTED_TARGET libudev)

enable_testing()

add_subdirectory(src)
