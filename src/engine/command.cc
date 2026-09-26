// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/command.h"

#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdlib.h>
#include <sys/mount.h>
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

// A step the child performs between fork and exec. Its failure is reported to
// the parent over a dedicated pipe, because a forked child of a threaded
// process may not allocate and so cannot format a message itself.
enum class ChildStep : std::uint8_t {
  kSetsid,
  kOpenNull,
  kDup2,
  kUnshare,
  kFork,
  kMountPrivate,
  kMountProc,
  kChroot,
  kChdir,
  kExec,
};

struct ChildFailure {
  ChildStep step = ChildStep::kExec;
  int error = 0;
};

std::string_view ChildStepName(ChildStep step) {
  switch (step) {
    case ChildStep::kSetsid:
      return "setsid";
    case ChildStep::kOpenNull:
      return "open /dev/null";
    case ChildStep::kDup2:
      return "dup2";
    case ChildStep::kUnshare:
      return "unshare(CLONE_NEWPID | CLONE_NEWNS)";
    case ChildStep::kFork:
      return "fork into PID namespace";
    case ChildStep::kMountPrivate:
      return "mount / private";
    case ChildStep::kMountProc:
      return "mount /proc";
    case ChildStep::kChroot:
      return "chroot";
    case ChildStep::kChdir:
      return "chdir /";
    case ChildStep::kExec:
      return "execve";
  }
  return "spawn";
}

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
  UniqueFd output_fd;
};

// Everything the child needs, resolved before the fork so that the child side
// only makes async-signal-safe calls.
struct ChildParams {
  int output_read_fd = -1;
  int output_write_fd = -1;
  int status_fd = -1;
  const char* program = nullptr;
  char* const* argv = nullptr;
  char* const* envp = nullptr;
  const char* chroot_dir = nullptr;
  bool new_pid_namespace = false;
};

struct ChildSetup {
  std::optional<std::filesystem::path> chroot_dir;
  bool new_pid_namespace = false;
};

void KillSession(pid_t pid) { ::kill(-pid, SIGKILL); }

void Reap(pid_t pid) {
  int ignored = 0;
  while (::waitpid(pid, &ignored, 0) < 0 && errno == EINTR) {
  }
}

[[noreturn]] void ReportAndExit(int status_fd, ChildStep step, int error) {
  const ChildFailure failure{.step = step, .error = error};
  const ssize_t ignored = ::write(status_fd, &failure, sizeof(failure));
  static_cast<void>(ignored);
  ::_exit(kSpawnFailureStatus);
}

// Exits with `pid`'s outcome, so the process the caller waits on reports the
// exit status or signal of the program that actually ran.
[[noreturn]] void ForwardExit(pid_t pid) {
  int wait_status = 0;
  while (::waitpid(pid, &wait_status, 0) < 0) {
    if (errno != EINTR) {
      ::_exit(kSpawnFailureStatus);
    }
  }
  if (WIFSIGNALED(wait_status)) {
    const int signal_number = WTERMSIG(wait_status);
    ::signal(signal_number, SIG_DFL);
    ::raise(signal_number);
  }
  ::_exit(WEXITSTATUS(wait_status));
}

// Unshares the PID and mount namespaces and forks, so that the program to come
// is process 1 of the new namespace and sees a /proc that matches it. Returns
// only in that new process; the caller waits for it and exits.
void EnterPidNamespace(const ChildParams& params) {
  if (::unshare(CLONE_NEWPID | CLONE_NEWNS) != 0) {
    ReportAndExit(params.status_fd, ChildStep::kUnshare, errno);
  }
  const pid_t pid = ::fork();
  if (pid < 0) {
    ReportAndExit(params.status_fd, ChildStep::kFork, errno);
  }
  if (pid == 0) {
    if (::mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0) {
      ReportAndExit(params.status_fd, ChildStep::kMountPrivate, errno);
    }
    if (::mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC,
                nullptr) != 0) {
      ReportAndExit(params.status_fd, ChildStep::kMountProc, errno);
    }
    return;
  }
  // Let the parent see the status pipe close as soon as the new process execs.
  ::close(params.status_fd);
  ForwardExit(pid);
}

[[noreturn]] void RunChild(const ChildParams& params) {
  const auto fail = [&params](ChildStep step) {
    ReportAndExit(params.status_fd, step, errno);
  };

  if (::setsid() < 0) {
    fail(ChildStep::kSetsid);
  }
  ::close(params.output_read_fd);
  const int null_fd = ::open("/dev/null", O_RDONLY);
  if (null_fd < 0) {
    fail(ChildStep::kOpenNull);
  }
  if (::dup2(null_fd, STDIN_FILENO) < 0 ||
      ::dup2(params.output_write_fd, STDOUT_FILENO) < 0 ||
      ::dup2(params.output_write_fd, STDERR_FILENO) < 0) {
    fail(ChildStep::kDup2);
  }
  if (null_fd != STDIN_FILENO) {
    ::close(null_fd);
  }
  if (params.output_write_fd > STDERR_FILENO) {
    ::close(params.output_write_fd);
  }
  if (params.new_pid_namespace) {
    EnterPidNamespace(params);
  }
  if (params.chroot_dir != nullptr) {
    if (::chroot(params.chroot_dir) != 0) {
      fail(ChildStep::kChroot);
    }
    if (::chdir("/") != 0) {
      fail(ChildStep::kChdir);
    }
  }
  ::execve(params.program, params.argv, params.envp);
  ReportAndExit(params.status_fd, ChildStep::kExec, errno);
}

// Reads the child's pre-exec failure, if any. The pipe is close-on-exec, so a
// successful exec arrives as end of file.
StatusOr<std::optional<ChildFailure>> ReadChildFailure(int status_fd) {
  ChildFailure failure{};
  ssize_t count = 0;
  while ((count = ::read(status_fd, &failure, sizeof(failure))) < 0) {
    const int error = errno;
    if (error != EINTR) {
      return ErrnoToStatus(error, "read spawn status");
    }
  }
  if (count != static_cast<ssize_t>(sizeof(failure))) {
    return std::optional<ChildFailure>();
  }
  return std::optional<ChildFailure>(failure);
}

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

std::vector<char*> BuildArgv(const std::string& program,
                             const std::vector<std::string>& args) {
  std::vector<char*> argv;
  argv.reserve(args.size() + 2);
  argv.push_back(const_cast<char*>(program.c_str()));
  for (const std::string& argument : args) {
    argv.push_back(const_cast<char*>(argument.c_str()));
  }
  argv.push_back(nullptr);
  return argv;
}

std::vector<char*> BuildEnvp(const std::vector<std::string>& environment) {
  std::vector<char*> envp;
  envp.reserve(environment.size() + 1);
  for (const std::string& entry : environment) {
    envp.push_back(const_cast<char*>(entry.data()));
  }
  envp.push_back(nullptr);
  return envp;
}

StatusOr<Child> Spawn(
    const std::string& program, const std::vector<std::string>& args,
    const std::vector<std::pair<std::string, std::string>>& env,
    const ChildSetup& setup) {
  const std::string resolved = ResolveProgram(program);
  const std::vector<std::string> environment = MakeEnvironment(env);
  const std::vector<char*> argv = BuildArgv(program, args);
  const std::vector<char*> envp = BuildEnvp(environment);

  PUU_ASSIGN_OR_RETURN(Pipe output_pipe, CreatePipe());
  PUU_ASSIGN_OR_RETURN(Pipe status_pipe, CreatePipe());

  const ChildParams params{
      .output_read_fd = output_pipe.read.Get(),
      .output_write_fd = output_pipe.write.Get(),
      .status_fd = status_pipe.write.Get(),
      .program = resolved.c_str(),
      .argv = argv.data(),
      .envp = envp.data(),
      .chroot_dir = setup.chroot_dir ? setup.chroot_dir->c_str() : nullptr,
      .new_pid_namespace = setup.new_pid_namespace,
  };

  const pid_t pid = ::fork();
  if (pid < 0) {
    const int error = errno;
    return ErrnoToStatus(error, fmt::format("fork for {}", program));
  }
  if (pid == 0) {
    RunChild(params);
  }

  output_pipe.write.Reset();
  status_pipe.write.Reset();
  const StatusOr<std::optional<ChildFailure>> failure =
      ReadChildFailure(status_pipe.read.Get());
  status_pipe.read.Reset();
  if (!failure.ok() || failure->has_value()) {
    output_pipe.read.Reset();
    Reap(pid);
    if (!failure.ok()) {
      return failure.status();
    }
    return ErrnoToStatus(
        (*failure)->error,
        fmt::format("{} for {}", ChildStepName((*failure)->step), program));
  }
  return Child{.pid = pid, .output_fd = std::move(output_pipe.read)};
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

Command& Command::NewPidNamespace() {
  new_pid_namespace_ = true;
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

  PUU_ASSIGN_OR_RETURN(
      Child child,
      Spawn(program_, args_, env_,
            ChildSetup{.chroot_dir = chroot_,
                       .new_pid_namespace = new_pid_namespace_}));

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

  const Status pumped = PumpOutput(child.output_fd.Get(), on_line, on_idle);
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
  PUU_ASSIGN_OR_RETURN(Child child,
                       Spawn(program_name, args, {}, ChildSetup{}));

  std::string output;
  const Status pumped = PumpOutput(
      child.output_fd.Get(),
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

Status RunInteractive(const std::filesystem::path& program,
                      const std::vector<std::string>& args) {
  const std::string resolved = ResolveProgram(program.string());
  const std::vector<std::string> environment = MakeEnvironment({});
  const std::vector<char*> envp = BuildEnvp(environment);
  const std::vector<char*> argv = BuildArgv(resolved, args);

  struct sigaction sa_ignore{}, sa_orig_int{}, sa_orig_quit{};
  sa_ignore.sa_handler = SIG_IGN;
  ::sigaction(SIGINT, &sa_ignore, &sa_orig_int);
  ::sigaction(SIGQUIT, &sa_ignore, &sa_orig_quit);

  const pid_t pid = ::fork();
  if (pid < 0) {
    const int error = errno;
    ::sigaction(SIGINT, &sa_orig_int, nullptr);
    ::sigaction(SIGQUIT, &sa_orig_quit, nullptr);
    return ErrnoToStatus(error, "fork");
  }

  if (pid == 0) {
    ::sigaction(SIGINT, &sa_orig_int, nullptr);
    ::sigaction(SIGQUIT, &sa_orig_quit, nullptr);
    sigset_t set;
    ::sigemptyset(&set);
    ::sigprocmask(SIG_SETMASK, &set, nullptr);

    ::execve(resolved.c_str(), argv.data(), envp.data());
    ::_exit(kSpawnFailureStatus);
  }

  int status = 0;
  pid_t waited = 0;
  while ((waited = ::waitpid(pid, &status, 0)) < 0) {
    if (errno != EINTR) {
      break;
    }
  }

  ::sigaction(SIGINT, &sa_orig_int, nullptr);
  ::sigaction(SIGQUIT, &sa_orig_quit, nullptr);

  if (waited < 0) {
    return ErrnoToStatus(errno, "waitpid");
  }
  if (WIFEXITED(status) && WEXITSTATUS(status) == kSpawnFailureStatus) {
    return NotFoundError("failed to execute {}", resolved);
  }
  if (WIFEXITED(status) && WEXITSTATUS(status) != 0) {
    return InternalError("{} exited with code {}", resolved,
                         WEXITSTATUS(status));
  }
  if (WIFSIGNALED(status)) {
    return InternalError("{} terminated by signal {}", resolved,
                         WTERMSIG(status));
  }
  return absl::OkStatus();
}

}  // namespace puu::engine
