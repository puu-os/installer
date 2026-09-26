// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "cli.h"

#include <array>
#include <cstdio>

#include <argtable3.h>

#include "base/status.h"
#include "config.h"

namespace puu {
namespace {

constexpr const char* kProgramName = "puu-installer";

template <size_t N>
class ArgTable {
 public:
  template <typename... Entries>
    requires(sizeof...(Entries) == N)
  explicit ArgTable(Entries*... entries)
      : entries_{static_cast<void*>(entries)...} {}

  ~ArgTable() { arg_freetable(entries_.data(), entries_.size()); }

  ArgTable(const ArgTable&) = delete;
  ArgTable& operator=(const ArgTable&) = delete;

  void** Data() { return entries_.data(); }

 private:
  std::array<void*, N> entries_;
};

template <typename... Entries>
ArgTable(Entries*...) -> ArgTable<sizeof...(Entries)>;

void PrintHelp(void** table) {
  std::printf("Usage: %s", kProgramName);
  arg_print_syntax(stdout, table, "\n");
  std::printf(
      "\nStandalone installer for bootc-based operating systems.\n\n"
      "Configuration is read from the file given by --config-path, or from\n"
      "the PUU_INSTALLER_CONFIG environment variable when the option is\n"
      "omitted. --config-path takes precedence.\n\n"
      "Options:\n");
  arg_print_glossary(stdout, table, "  %-26s %s\n");
  std::printf(
      "\nThe installer must run as root and rewrites the selected drive.\n");
}

}  // namespace

ParseResult ParseCommandLine(int argc, char** argv) {
  arg_lit* help = arg_litn("h", "help", 0, 1, "print this help and exit");
  arg_lit* version =
      arg_litn("V", "version", 0, 1, "print version information and exit");
  arg_file* config_path =
      arg_filen(nullptr, "config-path", "<file>", 0, 1,
                "read installer configuration from this YAML file");
  arg_file* source_image =
      arg_filen(nullptr, "source-image", "<path>", 0, 1,
                "override the install source with a bootc image directory or "
                "oci-archive");
  arg_str* k3s_role =
      arg_strn(nullptr, "k3s-role", "server|agent", 0, 1,
               "override the k3s cluster role from the configuration");
  arg_file* k3s_token_file =
      arg_filen(nullptr, "k3s-token-file", "<file>", 0, 1,
                "read the k3s cluster token from this file");

  struct arg_end* end = arg_end(20);

  ArgTable table(help, version, config_path, source_image, k3s_role,
                 k3s_token_file, end);

  ParseResult result;

  if (arg_nullcheck(table.Data()) != 0) {
    std::fprintf(stderr, "%s: cannot allocate the argument table\n",
                 kProgramName);
    return result;
  }

  const int errors = arg_parse(argc, argv, table.Data());

  if (help->count > 0) {
    PrintHelp(table.Data());
    result.outcome = ParseOutcome::kExitSuccess;
    return result;
  }

  if (version->count > 0) {
    std::printf("%s %s\n", kProgramName, PUU_INSTALLER_VERSION);
    result.outcome = ParseOutcome::kExitSuccess;
    return result;
  }

  if (errors > 0) {
    arg_print_errors(stderr, end, kProgramName);
    std::fprintf(stderr, "Usage: %s", kProgramName);
    arg_print_syntax(stderr, table.Data(), "\n");
    std::fprintf(stderr, "Try '%s --help' for more information.\n",
                 kProgramName);
    return result;
  }

  if (config_path->count > 0) {
    result.options.config_path = config_path->filename[0];
  }
  if (source_image->count > 0) {
    result.options.source_image = source_image->filename[0];
  }
  if (k3s_token_file->count > 0) {
    result.options.k3s_token_file = k3s_token_file->filename[0];
  }
  if (k3s_role->count > 0) {
    StatusOr<K3sRole> role = ParseK3sRole(k3s_role->sval[0]);
    if (!role.ok()) {
      const auto message = role.status().message();
      std::fprintf(stderr, "%s: --k3s-role: %.*s\n", kProgramName,
                   static_cast<int>(message.size()), message.data());
      return result;
    }
    result.options.k3s_role = *role;
  }

  result.outcome = ParseOutcome::kRun;
  return result;
}

}  // namespace puu
