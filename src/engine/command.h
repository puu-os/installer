// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_COMMAND_H_
#define PUU_ENGINE_COMMAND_H_

#include <chrono>
#include <filesystem>
#include <functional>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "base/status.h"
#include "engine/progress.h"

namespace puu::engine {

// Runs an external program, streaming its merged stdout/stderr into the log.
//
// The child is started in its own session with stdin closed, so a helper that
// tries to prompt fails instead of hanging the installer.
class Command {
 public:
  explicit Command(std::string program);

  Command& Arg(std::string argument);
  Command& Args(const std::vector<std::string>& arguments);
  Command& Env(std::string name, std::string value);

  // Runs the child inside `root` with the working directory set to "/".
  Command& Chroot(std::filesystem::path root);

  // Logs `message` every `interval` while the child is still running, so a slow
  // step does not look hung.
  Command& Heartbeat(std::chrono::seconds interval, std::string message);

  // Attaches a progress estimator. `emit` receives fractions in [0, 1].
  Command& Track(ProgressSource* source, std::function<void(double)> emit);

  // Executes and waits. Returns an error describing the exit status or signal,
  // with the tail of the child's output attached.
  //
  // Arguments that follow a secret-bearing option such as --token are
  // redacted from the logged command line.
  Status Run();

 private:
  std::string program_;
  std::vector<std::string> args_;
  std::vector<std::pair<std::string, std::string>> env_;
  std::optional<std::filesystem::path> chroot_;
  std::optional<std::chrono::seconds> heartbeat_interval_;
  std::string heartbeat_message_;
  ProgressSource* progress_source_ = nullptr;
  std::function<void(double)> progress_emit_;
};

// Runs `program` with `args` and returns its trimmed standard output. Used for
// short queries such as reading a filesystem UUID.
StatusOr<std::string> CaptureOutput(std::string_view program,
                                    const std::vector<std::string>& args);

}  // namespace puu::engine

#endif  // PUU_ENGINE_COMMAND_H_
