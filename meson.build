# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

project(
  'puu-installer',
  'cpp',
  version: '1.0.50',
  license: 'GPL-2.0-or-later',
  meson_version: '>= 1.1.0',
  default_options: [
    'cpp_std=c++20',
    'warning_level=3',
    'b_ndebug=if-release',
    'b_pie=true',
    'wrap_mode=forcefallback',
    # Vendored fallbacks are private implementation details: always static,
    # and their headers/libs are never installed (see subprojects/packagefiles).
    'fmt:default_library=static',
    'spdlog:default_library=static',
    'argtable3:default_library=static',
    'yaml-cpp:default_library=static',
    'ftxui:examples=disabled',
    'yaml-cpp:tests=disabled',
    'yaml-cpp:tools=false',
  ],
)

cxx = meson.get_compiler('cpp')

extra_warnings = cxx.get_supported_arguments(
  '-Wconversion',
  '-Wsign-conversion',
  '-Wshadow',
  '-Wnon-virtual-dtor',
  '-Wold-style-cast',
  '-Woverloaded-virtual',
  '-Wdouble-promotion',
  '-Wimplicit-fallthrough',
  '-Wno-missing-field-initializers',
  '-Wformat=2',
)
add_project_arguments(extra_warnings, language: 'cpp')

if cxx.has_argument('-fstack-protector-strong')
  add_project_arguments('-fstack-protector-strong', language: 'cpp')
endif

if get_option('optimization') != '0'
  add_project_arguments('-D_FORTIFY_SOURCE=2', language: 'cpp')
endif

argtable3_dep = dependency('argtable3', include_type: 'system')
yaml_cpp_dep = dependency('yaml-cpp', include_type: 'system')
fmt_dep = dependency('fmt', include_type: 'system')
spdlog_dep = dependency('spdlog', include_type: 'system')
json_dep = dependency('nlohmann_json', include_type: 'system')

absl_deps = [
  dependency('absl_status', include_type: 'system'),
  dependency('absl_strings', include_type: 'system'),
  dependency('absl_base', include_type: 'system'),
  dependency('absl_synchronization', include_type: 'system'),
  dependency('absl_time', include_type: 'system'),
]

ftxui_deps = [
  dependency('ftxui-screen', include_type: 'system'),
  dependency('ftxui-dom', include_type: 'system'),
  dependency('ftxui-component', include_type: 'system'),
]

openssl_dep = dependency('openssl', version: '>= 3.0', include_type: 'system')
blkid_dep = dependency('blkid', include_type: 'system')
udev_dep = dependency('libudev', include_type: 'system')
threads_dep = dependency('threads')

installer_deps = [
  argtable3_dep,
  yaml_cpp_dep,
  fmt_dep,
  spdlog_dep,
  json_dep,
  openssl_dep,
  blkid_dep,
  udev_dep,
  threads_dep,
] + absl_deps

subdir('src')
