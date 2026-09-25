# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

cmake_minimum_required(VERSION 3.31)

project(
  puu-installer
  VERSION 1.0.58
  LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
set(CMAKE_CXX_SCAN_FOR_MODULES OFF)

if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Debug CACHE STRING "Build type" FORCE)
endif()

set(CMAKE_POSITION_INDEPENDENT_CODE ON)

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

include(FetchContent)

set(ARGTABLE3_ENABLE_TESTS OFF CACHE BOOL "" FORCE)
set(ARGTABLE3_ENABLE_EXAMPLES OFF CACHE BOOL "" FORCE)
FetchContent_Declare(
  argtable3
  URL https://github.com/argtable/argtable3/releases/download/v3.3.1/argtable-v3.3.1.tar.gz
  URL_HASH SHA256=c08bca4b88ddb9234726b75455b3b1670d7c864d8daf198eaa7a3b4d41addf2c
  SYSTEM
  EXCLUDE_FROM_ALL)

FetchContent_MakeAvailable(argtable3)

find_package(absl REQUIRED)
find_package(fmt REQUIRED)
find_package(ftxui REQUIRED)
find_package(nlohmann_json REQUIRED)
find_package(OpenSSL 3.0 REQUIRED)
find_package(spdlog REQUIRED)
find_package(Threads REQUIRED)
find_package(yaml-cpp REQUIRED)
find_package(PkgConfig REQUIRED)
pkg_check_modules(BLKID REQUIRED IMPORTED_TARGET blkid)
pkg_check_modules(UDEV REQUIRED IMPORTED_TARGET libudev)
pkg_check_modules(MOUNT REQUIRED IMPORTED_TARGET mount)

enable_testing()

add_subdirectory(src)
