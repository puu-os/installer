// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "tui/logging.h"

#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <cerrno>
#include <cstddef>
#include <deque>
#include <memory>
#include <mutex>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <absl/base/no_destructor.h>
#include <spdlog/details/log_msg.h>
#include <spdlog/sinks/base_sink.h>
#include <spdlog/spdlog.h>

#include "base/status.h"
#include "engine/pipeline.h"

namespace puu::tui {

std::string TruncateUtf8(std::string_view text, size_t max_bytes) {
  if (text.size() <= max_bytes) return std::string(text);
  size_t end = max_bytes;
  while (end > 0 && (static_cast<unsigned char>(text[end]) & 0xC0U) == 0x80U) {
    --end;
  }
  return std::string(text.substr(0, end));
}

namespace {

constexpr const char* kLogDirectory = "/run/puu-installer";
constexpr const char* kLogPath = "/run/puu-installer/install.log";

constexpr size_t kMaxLineBytes = 1024;

constexpr size_t kMaxLines = 2000;

class LineBuffer {
 public:
  static LineBuffer& Instance() {
    static absl::NoDestructor<LineBuffer> buffer;
    return *buffer;
  }

  void Push(std::string line) {
    const std::lock_guard<std::mutex> lock(mutex_);
    lines_.push_back(std::move(line));
    if (lines_.size() > kMaxLines) lines_.pop_front();
  }

  std::vector<std::string> Snapshot() {
    const std::lock_guard<std::mutex> lock(mutex_);
    return {lines_.begin(), lines_.end()};
  }

 private:
  std::mutex mutex_;
  std::deque<std::string> lines_;
};

class DescriptorSink final : public spdlog::sinks::base_sink<std::mutex> {
 public:
  explicit DescriptorSink(int descriptor) : descriptor_(descriptor) {}

  ~DescriptorSink() override {
    if (descriptor_ >= 0) ::close(descriptor_);
  }

  DescriptorSink(const DescriptorSink&) = delete;
  DescriptorSink& operator=(const DescriptorSink&) = delete;

 protected:
  void sink_it_(const spdlog::details::log_msg& message) override {
    spdlog::memory_buf_t formatted;
    formatter_->format(message, formatted);
    Write(std::string_view(formatted.data(), formatted.size()));
  }

  void flush_() override {}

 private:
  void Write(std::string_view text) {
    while (descriptor_ >= 0 && !text.empty()) {
      const ssize_t written = ::write(descriptor_, text.data(), text.size());
      if (written < 0) {
        if (errno == EINTR) continue;

        ::close(descriptor_);
        descriptor_ = -1;
        return;
      }
      text.remove_prefix(static_cast<size_t>(written));
    }
  }

  int descriptor_;
};

class ScreenSink final : public spdlog::sinks::base_sink<std::mutex> {
 protected:
  void sink_it_(const spdlog::details::log_msg& message) override {
    const std::string_view logger(message.logger_name.data(),
                                  message.logger_name.size());
    if (logger != engine::kInstallationLogTarget) return;
    if (message.level < spdlog::level::info) return;
    std::string_view payload(message.payload.data(), message.payload.size());
    while (!payload.empty()) {
      const size_t newline = payload.find('\n');
      std::string_view line = payload.substr(0, newline);
      if (!line.empty() && line.back() == '\r') line.remove_suffix(1);
      LineBuffer::Instance().Push(TruncateUtf8(line, kMaxLineBytes));
      if (newline == std::string_view::npos) break;
      payload.remove_prefix(newline + 1);
    }
  }

  void flush_() override {}
};

StatusOr<int> OpenLogFile() {
  if (::mkdir(kLogDirectory, 0700) != 0 && errno != EEXIST) {
    return ErrnoToStatus(errno, "mkdir /run/puu-installer");
  }

  if (::chmod(kLogDirectory, 0700) != 0) {
    return ErrnoToStatus(errno, "chmod /run/puu-installer");
  }
  const int opened = ::open(
      kLogPath, O_WRONLY | O_CREAT | O_TRUNC | O_NOFOLLOW | O_CLOEXEC, 0600);
  if (opened < 0) {
    return ErrnoToStatus(errno, "open /run/puu-installer/install.log");
  }
  if (::fchmod(opened, 0600) != 0) {
    const int error = errno;
    ::close(opened);
    return ErrnoToStatus(error, "fchmod /run/puu-installer/install.log");
  }
  return opened;
}

}  // namespace

Status Init() {
  PUU_ASSIGN_OR_RETURN(const int descriptor, OpenLogFile());

  try {
    auto file_sink = std::make_shared<DescriptorSink>(descriptor);

    file_sink->set_pattern("%Y-%m-%dT%H:%M:%S.%e %l %v");
    const std::vector<spdlog::sink_ptr> sinks{std::move(file_sink),
                                              std::make_shared<ScreenSink>()};

    auto logger =
        std::make_shared<spdlog::logger>("puu", sinks.begin(), sinks.end());
    logger->set_level(spdlog::level::debug);
    spdlog::set_default_logger(logger);

    auto installation = std::make_shared<spdlog::logger>(
        std::string(engine::kInstallationLogTarget), sinks.begin(),
        sinks.end());
    installation->set_level(spdlog::level::debug);
    spdlog::register_logger(installation);
  } catch (const spdlog::spdlog_ex& error) {
    return InternalError("could not configure logging: {}", error.what());
  }
  return absl::OkStatus();
}

std::vector<std::string> SnapshotLogLines() {
  return LineBuffer::Instance().Snapshot();
}

}  // namespace puu::tui
