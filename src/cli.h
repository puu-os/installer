// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_CLI_H_
#define PUU_CLI_H_

#include <cstdint>
#include <filesystem>
#include <optional>

#include "config.h"

namespace puu {

// Options accepted on the command line. Everything else is configured through
// the YAML configuration file.
struct CommandLine {
  // Configuration file. Falls back to $PUU_INSTALLER_CONFIG when absent.
  std::optional<std::filesystem::path> config_path;
  // Overrides automatic discovery of the local bootc source image.
  std::optional<std::filesystem::path> source_image;
  // Overrides `k3s.role` from the configuration file.
  std::optional<K3sRole> k3s_role;
  // File whose first line is the k3s cluster token.
  std::optional<std::filesystem::path> k3s_token_file;
};

enum class ParseOutcome : std::uint8_t {
  kRun,
  kExitSuccess,
  kExitFailure,
};

struct ParseResult {
  ParseOutcome outcome = ParseOutcome::kExitFailure;
  CommandLine options;
};

// Parses `argv` using argtable3. Help, usage, version and syntax-error output
// are written to stdout/stderr here, so the caller only decides an exit code.
ParseResult ParseCommandLine(int argc, char** argv);

}  // namespace puu

#endif  // PUU_CLI_H_
