// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/command.h"

#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdlib.h>
#include <sys/poll.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <chrono>
#include <cstdlib>
#include <ctime>
#include <deque>
#include <filesystem>
#include <functional>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <absl/cleanup/cleanup.h>
#include <absl/status/status.h>
#include <absl/strings/str_join.h>
#include <fmt/format.h>
#include <spdlog/logger.h>

#include "base/status.h"
#include "engine/pipeline.h"
#include "engine/progress.h"
#include "engine/util.h"

namespace puu::engine {
namespace {

constexpr std::array<std::string_view, 4> kSecretOptions = {
    "--password", "--passphrase", "--secret", "--token"};

constexpr std::string_view kRedacted = "<redacted>";

constexpr size_t kOutputTailLines = 12;

constexpr int kSpawnFailureStatus = 127;

constexpr std::chrono::milliseconds kProgressSampleInterval{500};

constexpr int kPollTimeoutMs = 200;

bool IsSecretOption(std::string_view argument) {
  return std::ranges::find(kSecretOptions, argument) != kSecretOptions.end();
}

std::string FormatArgv(std::string_view program,
                       const std::vector<std::string>& args) {
  std::string out(program);
  bool redact_next = false;
  for (const std::string& argument : args) {
    out.push_back(' ');
    if (redact_next) {
      out.append(kRedacted);
      redact_next = false;
      continue;
    }
    if (IsSecretOption(argument)) {
      out.append(argument);
      redact_next = true;
      continue;
    }
    const size_t equals = argument.find('=');
    if (equals != std::string::npos &&
        IsSecretOption(std::string_view(argument).substr(0, equals))) {
      out.append(argument, 0, equals + 1).append(kRedacted);
      continue;
    }
    out.append(argument);
  }
  return out;
}

struct Child {
  pid_t pid = -1;
  int output_fd = -1;
};

void KillSession(pid_t pid) { ::kill(-pid, SIGKILL); }

std::string ResolveProgram(const std::string& program) {
  if (program.find('/') != std::string::npos) {
    return program;
  }

  const std::optional<std::filesystem::path> resolved = FindInPath(program);
  return resolved.has_value() ? resolved->string() : program;
}

std::vector<std::string> MakeEnvironment(
    const std::vector<std::pair<std::string, std::string>>& env) {
  const auto overridden = [&env](std::string_view entry) {
    return std::ranges::any_of(env, [entry](const auto& pair) {
      const std::string_view name(pair.first);
      return entry.size() > name.size() && entry.starts_with(name) &&
             entry[name.size()] == '=';
    });
  };
  size_t inherited = 0;
  for (char** entry = environ; *entry != nullptr; ++entry) {
    ++inherited;
  }
  std::vector<std::string> entries;
  entries.reserve(inherited + env.size());
  for (char** entry = environ; *entry != nullptr; ++entry) {
    if (!overridden(*entry)) {
      entries.emplace_back(*entry);
    }
  }
  for (const auto& [name, value] : env) {
    entries.push_back(fmt::format("{}={}", name, value));
  }
  return entries;
}

StatusOr<Child> Spawn(
    const std::string& program, const std::vector<std::string>& args,
    const std::vector<std::pair<std::string, std::string>>& env,
    const std::optional<std::filesystem::path>& chroot_dir) {
  std::vector<char*> argv;
  argv.reserve(args.size() + 2);
  argv.push_back(const_cast<char*>(program.c_str()));
  for (const std::string& argument : args) {
    argv.push_back(const_cast<char*>(argument.c_str()));
  }
  argv.push_back(nullptr);
  const std::string resolved = ResolveProgram(program);
  const std::vector<std::string> environment = MakeEnvironment(env);
  std::vector<char*> envp;
  envp.reserve(environment.size() + 1);
  for (const std::string& entry : environment) {
    envp.push_back(const_cast<char*>(entry.data()));
  }
  envp.push_back(nullptr);
  const std::string failure_message =
      fmt::format("failed to start {}\n", program);

  std::array<int, 2> pipe_fds = {-1, -1};

  if (::pipe2(pipe_fds.data(), O_CLOEXEC) != 0) {
    const int error = errno;
    return ErrnoToStatus(error, "pipe");
  }

  const pid_t pid = ::fork();
  if (pid < 0) {
    const int error = errno;
    ::close(pipe_fds[0]);
    ::close(pipe_fds[1]);
    return ErrnoToStatus(error, fmt::format("fork for {}", program));
  }

  if (pid == 0) {
    const auto fail = [&failure_message]() {
      const ssize_t ignored = ::write(STDOUT_FILENO, failure_message.data(),
                                      failure_message.size());
      static_cast<void>(ignored);
      ::_exit(kSpawnFailureStatus);
    };

    if (::setsid() < 0) {
      fail();
    }
    ::close(pipe_fds[0]);
    const int null_fd = ::open("/dev/null", O_RDONLY);
    if (null_fd < 0) {
      fail();
    }
    if (::dup2(null_fd, STDIN_FILENO) < 0) {
      fail();
    }
    if (::dup2(pipe_fds[1], STDOUT_FILENO) < 0) {
      fail();
    }
    if (::dup2(pipe_fds[1], STDERR_FILENO) < 0) {
      fail();
    }
    if (null_fd != STDIN_FILENO) {
      ::close(null_fd);
    }
    if (pipe_fds[1] > STDERR_FILENO) {
      ::close(pipe_fds[1]);
    }
    if (chroot_dir) {
      if (::chroot(chroot_dir->c_str()) != 0) {
        fail();
      }
      if (::chdir("/") != 0) {
        fail();
      }
    }
    ::execve(resolved.c_str(), argv.data(), envp.data());
    fail();
  }

  ::close(pipe_fds[1]);
  return Child{.pid = pid, .output_fd = pipe_fds[0]};
}

StatusOr<std::optional<std::string>> WaitForChild(pid_t pid,
                                                  std::string_view program) {
  int wait_status = 0;
  while (::waitpid(pid, &wait_status, 0) < 0) {
    const int error = errno;
    if (error == EINTR) {
      continue;
    }
    return ErrnoToStatus(error, fmt::format("wait for {}", program));
  }
  if (WIFSIGNALED(wait_status)) {
    return std::optional<std::string>(
        fmt::format("killed by signal {}", WTERMSIG(wait_status)));
  }
  const int code = WEXITSTATUS(wait_status);
  if (code == 0) {
    return std::optional<std::string>();
  }
  return std::optional<std::string>(fmt::format("exited with status {}", code));
}

Status PumpOutput(int fd, const std::function<void(std::string_view)>& on_line,
                  const std::function<void()>& on_idle) {
  std::string pending;
  std::array<char, 4096> buffer = {};
  while (true) {
    pollfd poll_fd = {.fd = fd, .events = POLLIN, .revents = 0};
    if (::poll(&poll_fd, 1, kPollTimeoutMs) < 0) {
      const int error = errno;
      if (error == EINTR) {
        continue;
      }
      return ErrnoToStatus(error, "poll command output");
    }
    if (poll_fd.revents != 0) {
      const ssize_t count = ::read(fd, buffer.data(), buffer.size());
      if (count < 0) {
        const int error = errno;
        if (error == EINTR || error == EAGAIN) {
          continue;
        }
        return ErrnoToStatus(error, "read command output");
      }
      if (count == 0) {
        break;
      }
      pending.append(buffer.data(), static_cast<size_t>(count));
      size_t start = 0;
      for (size_t newline = pending.find('\n', start);
           newline != std::string::npos; newline = pending.find('\n', start)) {
        on_line(std::string_view(pending).substr(start, newline - start));
        start = newline + 1;
      }
      pending.erase(0, start);
    }
    on_idle();
  }

  if (!pending.empty()) {
    on_line(pending);
  }
  return absl::OkStatus();
}

}  // namespace

Command::Command(std::string program) : program_(std::move(program)) {}

Command& Command::Arg(std::string argument) {
  args_.push_back(std::move(argument));
  return *this;
}

Command& Command::Args(const std::vector<std::string>& arguments) {
  args_.insert(args_.end(), arguments.begin(), arguments.end());
  return *this;
}

Command& Command::Env(std::string name, std::string value) {
  env_.emplace_back(std::move(name), std::move(value));
  return *this;
}

Command& Command::Chroot(std::filesystem::path root) {
  chroot_ = std::move(root);
  return *this;
}

Command& Command::Heartbeat(std::chrono::seconds interval,
                            std::string message) {
  heartbeat_interval_ = interval;
  heartbeat_message_ = std::move(message);
  return *this;
}

Command& Command::Track(ProgressSource* source,
                        std::function<void(double)> emit) {
  progress_source_ = source;
  progress_emit_ = std::move(emit);
  return *this;
}

Status Command::Run() {
  const std::string command_line = FormatArgv(program_, args_);
  const std::shared_ptr<spdlog::logger> logger = InstallationLogger();
  logger->info("$ {}", command_line);

  PUU_ASSIGN_OR_RETURN(const Child child,
                       Spawn(program_, args_, env_, chroot_));
  const absl::Cleanup close_output = [&child] { ::close(child.output_fd); };

  std::deque<std::string> tail;
  const auto started = std::chrono::steady_clock::now();
  auto last_heartbeat = started;
  auto last_sample = started;

  const auto on_line = [this, &logger, &tail](std::string_view raw) {
    const std::string line = CleanLogLine(raw);
    if (line.empty()) {
      return;
    }
    if (progress_source_ != nullptr) {
      progress_source_->ObserveLine(line);
    }
    if (tail.size() >= kOutputTailLines) {
      tail.pop_front();
    }
    tail.push_back(line);
    logger->info("{}", line);
  };

  const auto on_idle = [this, &logger, started, &last_heartbeat,
                        &last_sample]() {
    const auto now = std::chrono::steady_clock::now();
    if (heartbeat_interval_ && now - last_heartbeat >= *heartbeat_interval_) {
      const auto elapsed =
          std::chrono::duration_cast<std::chrono::seconds>(now - started);
      logger->info("{} ({}s elapsed)", heartbeat_message_, elapsed.count());
      last_heartbeat = now;
    }
    if (progress_source_ != nullptr && progress_emit_ &&
        now - last_sample >= kProgressSampleInterval) {
      if (const std::optional<double> fraction = progress_source_->Fraction()) {
        progress_emit_(*fraction);
      }
      last_sample = now;
    }
  };

  const Status pumped = PumpOutput(child.output_fd, on_line, on_idle);
  if (!pumped.ok()) {
    KillSession(child.pid);
  }
  PUU_ASSIGN_OR_RETURN(const std::optional<std::string> failure,
                       WaitForChild(child.pid, program_));
  PUU_RETURN_IF_ERROR(WithContext(pumped, command_line));

  if (!failure) {
    return absl::OkStatus();
  }
  if (tail.empty()) {
    return InternalError("command failed ({}): {}", *failure, command_line);
  }

  return InternalError("command failed ({}): {}\ncommand: {}", *failure,
                       absl::StrJoin(tail, "\n"), command_line);
}

StatusOr<std::string> CaptureOutput(std::string_view program,
                                    const std::vector<std::string>& args) {
  const std::string program_name(program);
  PUU_ASSIGN_OR_RETURN(const Child child,
                       Spawn(program_name, args, {}, std::nullopt));
  const absl::Cleanup close_output = [&child] { ::close(child.output_fd); };

  std::string output;
  const Status pumped = PumpOutput(
      child.output_fd,
      [&output](std::string_view line) { output.append(line).push_back('\n'); },
      [] {});
  if (!pumped.ok()) {
    KillSession(child.pid);
  }
  PUU_ASSIGN_OR_RETURN(const std::optional<std::string> failure,
                       WaitForChild(child.pid, program_name));
  PUU_RETURN_IF_ERROR(WithContext(pumped, program_name));

  if (failure) {
    return InternalError("{} failed ({}): {}", program_name, *failure,
                         Trim(output));
  }
  return std::string(Trim(output));
}

}  // namespace puu::engine
