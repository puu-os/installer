// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "tui/app.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <cctype>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <exception>
#include <filesystem>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include <absl/base/no_destructor.h>
#include <absl/strings/ascii.h>
#include <fmt/format.h>
#include <ftxui/component/component.hpp>
#include <ftxui/component/component_base.hpp>
#include <ftxui/component/component_options.hpp>
#include <ftxui/component/event.hpp>
#include <ftxui/component/screen_interactive.hpp>
#include <ftxui/dom/elements.hpp>
#include <ftxui/screen/color.hpp>
#include <openssl/crypto.h>
#include <spdlog/spdlog.h>

#include "base/status.h"
#include "config.h"
#include "engine/block.h"
#include "engine/context.h"
#include "engine/firmware.h"
#include "engine/pipeline.h"
#include "engine/step.h"
#include "engine/util.h"
#include "tui/logging.h"
#include "tui/timezone.h"

namespace puu::tui {
namespace {

using ftxui::Component;
using ftxui::Components;
using ftxui::Element;
using ftxui::Elements;
using ftxui::Event;

constexpr int kRebootDelaySeconds = 30;

constexpr std::chrono::milliseconds kTickInterval{200};

constexpr int kMinColumns = 64;
constexpr int kMinRows = 20;

constexpr int kLogPaneChrome = 12;
struct LocaleChoice {
  std::string_view label;
  std::string_view name;
};

constexpr std::array<LocaleChoice, 10> kLocales{{
    {.label = "English (US)", .name = "en_US.UTF-8"},
    {.label = "English (UK)", .name = "en_GB.UTF-8"},
    {.label = "Finnish", .name = "fi_FI.UTF-8"},
    {.label = "Swedish", .name = "sv_SE.UTF-8"},
    {.label = "Norwegian", .name = "nb_NO.UTF-8"},
    {.label = "Danish", .name = "da_DK.UTF-8"},
    {.label = "German", .name = "de_DE.UTF-8"},
    {.label = "French", .name = "fr_FR.UTF-8"},
    {.label = "Italian", .name = "it_IT.UTF-8"},
    {.label = "Spanish", .name = "es_ES.UTF-8"},
}};

constexpr std::array<std::string_view, 2> kShells{"Bash", "Zsh"};

enum class Route : std::uint8_t {
  kWelcome = 0,
  kTarget,
  kIdentity,
  kTimezone,
  kSecureBoot,
  kReview,
  kInstall,
  kReboot,
};

enum class ModalKind : std::uint8_t {
  kWipe,
  kRebootConfirm,
  kSetupMode,
  kFailure,
  kCancel,
};

enum class StepState : std::uint8_t {
  kPending,
  kActive,
  kDone,
  kSkipped,
  kFailed
};

enum class InstallState : std::uint8_t { kRunning, kSucceeded, kFailed };

struct DriveEntry {
  std::filesystem::path path;
  std::string label;
  bool busy = false;
  std::string busy_reason;
};

struct RouteInfo {
  std::string_view subtitle;
  std::string_view footer;
};

constexpr std::array<RouteInfo, 8> kRouteInfo = {{
    {.subtitle = "READY  /  GUIDED SYSTEM INSTALLATION",
     .footer = "Up/Down move   Enter select   Esc quit"},
    {.subtitle = "SETUP  1 / 5  /  TARGET DISK",
     .footer = "Up/Down move   Enter select   Ctrl-R rescan   Esc back"},
    {.subtitle = "SETUP  2 / 5  /  SYSTEM IDENTITY",
     .footer =
         "Tab next field   Left/Right change   Enter continue   Esc back"},
    {.subtitle = "SETUP  3 / 5  /  TIME ZONE",
     .footer = "Type to filter   Up/Down move   Enter select   Esc back"},
    {.subtitle = "SETUP  4 / 5  /  PLATFORM SECURITY",
     .footer = "Up/Down move   Enter select   Esc back"},
    {.subtitle = "SETUP  5 / 5  /  REVIEW",
     .footer = "Up/Down move   Enter select   Esc back"},
    {.subtitle = "INSTALLATION IN PROGRESS",
     .footer = "PgUp/PgDn scroll log   Esc cancel"},
    {.subtitle = "INSTALLATION COMPLETE",
     .footer = "Any key cancels the countdown   Enter select"},
}};

std::string_view Subtitle(Route route) {
  return kRouteInfo[static_cast<size_t>(route)].subtitle;
}

std::string_view Footer(Route route) {
  return kRouteInfo[static_cast<size_t>(route)].footer;
}

std::string_view StepMarker(StepState state) {
  switch (state) {
    case StepState::kPending:
      return "[ ]";
    case StepState::kActive:
      return "[>]";
    case StepState::kDone:
      return "[x]";
    case StepState::kSkipped:
      return "[-]";
    case StepState::kFailed:
      return "[!]";
  }
  return "[ ]";
}

ftxui::Color StepColor(StepState state) {
  switch (state) {
    case StepState::kPending:
      return ftxui::Color::GrayDark;
    case StepState::kActive:
      return ftxui::Color::Cyan;
    case StepState::kDone:
      return ftxui::Color::Green;
    case StepState::kSkipped:
      return ftxui::Color::GrayLight;
    case StepState::kFailed:
      return ftxui::Color::Red;
  }
  return ftxui::Color::Default;
}

std::string ToLower(std::string_view text) {
  return absl::AsciiStrToLower(absl::string_view(text.data(), text.size()));
}

const std::vector<std::string>& LoweredTimezones() {
  static const absl::NoDestructor<std::vector<std::string>> lowered([] {
    std::vector<std::string> values;
    values.reserve(Timezones().size());
    for (const std::string& zone : Timezones()) {
      values.push_back(ToLower(zone));
    }
    return values;
  }());
  return *lowered;
}

void Truncate(std::string* value, size_t max_bytes) {
  if (max_bytes == std::string::npos) {
    return;
  }
  *value = TruncateUtf8(*value, max_bytes);
}

void WipeString(std::string* value) {
  OPENSSL_cleanse(value->data(), value->size());
  value->clear();
}

std::string FrameSafe(std::string_view text) {
  std::string out;
  out.reserve(text.size());
  for (const char ch : text) {
    const auto raw = static_cast<unsigned char>(ch);
    out.push_back(raw < 0x20 || raw == 0x7f ? ' ' : ch);
  }
  return out;
}

Element Field(std::string_view label, Element value) {
  return ftxui::hbox({
      ftxui::text(std::string(label)) |
          ftxui::size(ftxui::WIDTH, ftxui::EQUAL, 18),
      std::move(value),
  });
}

[[nodiscard]] int LogPageLines() {
  const ftxui::ScreenInteractive* screen = ftxui::ScreenInteractive::Active();
  const int height = screen != nullptr ? screen->dimy() : kMinRows;
  return std::max(4, height - kLogPaneChrome);
}

}  // namespace

class Installer::Impl {
 public:
  explicit Impl(engine::Context context) : context_(std::move(context)) {}

  ~Impl() { StopWorkers(); }

  Impl(const Impl&) = delete;
  Impl& operator=(const Impl&) = delete;

  Trap Run();

  engine::Context TakeContext() { return std::move(context_); }

 private:
  [[nodiscard]] Route CurrentRoute() const {
    return static_cast<Route>(route_index_);
  }
  void Goto(Route route);

  Component BuildRoot();
  Component BuildWelcome();
  Component BuildTarget();
  Component BuildIdentity();
  Component BuildTimezone();
  Component BuildSecureBoot();
  Component BuildReview();
  Component BuildInstall();
  Component BuildReboot();
  Component BuildModal();

  [[nodiscard]] Element RenderChrome(Element body) const;
  [[nodiscard]] Element RenderInstall() const;
  [[nodiscard]] Element RenderSteps() const;
  [[nodiscard]] Element RenderLog() const;
  [[nodiscard]] std::string Title() const;

  bool OnGlobalEvent(const Event& event);
  void OnWelcomeEnter();
  void OnDriveEnter();
  void OnIdentitySubmit();
  void OnTimezoneEnter();
  void OnEnableSecureBoot();
  void OnTick();

  void ResetFlow();
  void RefreshDrives();
  void RefilterZones();
  [[nodiscard]] bool SetupModeOn() const;

  void OpenModal(ModalKind kind);
  void CloseModal(bool confirmed);

  void StartInstall();
  void RunInstall(engine::Context context);
  bool AwaitCommit(size_t step_index);
  void OnProgress(size_t step_index, double fraction);
  void OnStepSkipped(size_t step_index);
  void OnInstallFinished(const Status& status);
  [[nodiscard]] double OverallProgress() const;

  void SetTrap(Trap trap);
  void Post(std::function<void()> task);
  void StartTicker();
  void StopWorkers();

  engine::Context context_;
  Trap trap_ = Trap::kQuit;

  std::mutex screen_mutex_;
  ftxui::ScreenInteractive* screen_ = nullptr;

  int route_index_ = static_cast<int>(Route::kWelcome);

  int welcome_index_ = 0;

  std::vector<DriveEntry> drives_;
  std::vector<std::string> drive_rows_;
  std::string drive_error_;
  std::string target_notice_;
  int drive_index_ = 0;
  bool scanning_ = false;
  std::thread drive_thread_;

  int language_index_ = 0;
  int shell_index_ = 0;
  std::vector<std::string> language_entries_;
  std::vector<std::string> shell_entries_;
  std::string hostname_;
  std::string username_;
  std::string password_;
  std::string repeated_password_;
  bool reveal_password_ = false;
  bool hide_password_ = true;
  std::string validation_error_;

  std::string zone_filter_;
  std::vector<std::string> zone_rows_;
  int zone_index_ = 0;

  std::optional<engine::SetupMode> firmware_;

  bool modal_open_ = false;

  std::optional<ModalKind> pending_modal_;
  ModalKind modal_kind_ = ModalKind::kWipe;
  std::string modal_title_;
  std::string modal_body_;
  std::string modal_dismiss_label_ = "No";
  std::string modal_confirm_label_ = "Yes";

  std::thread install_thread_;
  std::atomic<bool> install_cancel_{false};
  InstallState install_state_ = InstallState::kRunning;
  bool install_committed_ = false;
  std::string install_error_;
  std::string install_notice_;
  std::vector<StepState> step_states_;
  std::vector<double> step_fractions_;
  size_t current_step_ = 0;

  size_t log_scroll_ = 0;
  std::chrono::steady_clock::time_point install_started_;
  std::chrono::seconds install_elapsed_{0};

  std::mutex commit_mutex_;
  std::condition_variable commit_signal_;
  bool commit_acknowledged_ = false;
  bool ui_finished_ = false;

  std::atomic<bool> ticker_stop_{false};
  std::atomic<bool> ticking_{false};
  std::thread ticker_thread_;

  bool reboot_automatic_ = false;
  int reboot_remaining_ = kRebootDelaySeconds;
  std::chrono::steady_clock::time_point reboot_deadline_;
  int reboot_index_ = 0;
};

Trap Installer::Impl::Run() {
  auto screen = ftxui::ScreenInteractive::Fullscreen();

  screen.ForceHandleCtrlC(false);
  {
    const std::lock_guard<std::mutex> lock(screen_mutex_);
    screen_ = &screen;
  }

  hostname_ = context_.config.identity.hostname.Value();
  username_ = context_.username.has_value() ? context_.username->Value() : "";
  shell_index_ = context_.user_shell == UserShell::kZsh ? 1 : 0;
  RefilterZones();

  Component const root = BuildRoot();
  StartTicker();

  const auto cleanup = [this] {
    {
      const std::lock_guard<std::mutex> lock(screen_mutex_);
      screen_ = nullptr;
    }
    StopWorkers();
    WipeString(&password_);
    WipeString(&repeated_password_);
  };
  try {
    screen.Loop(root);
  } catch (...) {
    cleanup();
    throw;
  }
  cleanup();
  return trap_;
}

Component Installer::Impl::BuildRoot() {
  Component const tab = ftxui::Container::Tab(
      Components{BuildWelcome(), BuildTarget(), BuildIdentity(),
                 BuildTimezone(), BuildSecureBoot(), BuildReview(),
                 BuildInstall(), BuildReboot()},
      &route_index_);
  Component const chrome =
      ftxui::Renderer(tab, [this, tab] { return RenderChrome(tab->Render()); });
  Component const root = chrome | ftxui::Modal(BuildModal(), &modal_open_);
  return ftxui::CatchEvent(
      root, [this](const Event& event) { return OnGlobalEvent(event); });
}

void Installer::Impl::Goto(Route route) {
  route_index_ = static_cast<int>(route);
  ticking_ = route == Route::kInstall || route == Route::kReboot;
}

bool Installer::Impl::OnGlobalEvent(const Event& event) {
  if (modal_open_) {
    return event == Event::CtrlC;
  }
  if (event != Event::CtrlC) {
    return false;
  }

  if (CurrentRoute() == Route::kInstall &&
      install_state_ == InstallState::kRunning) {
    if (install_committed_) {
      install_notice_ =
          "The target drive has been written; the installation can no longer "
          "be cancelled.";
    } else {
      OpenModal(ModalKind::kCancel);
    }
    return true;
  }
  SetTrap(Trap::kQuit);
  return true;
}

Component Installer::Impl::BuildWelcome() {
  ftxui::MenuOption option = ftxui::MenuOption::Vertical();
  option.entries = std::vector<std::string>{
      "Install system", "Open recovery shell", "Reboot machine"};
  option.selected = &welcome_index_;
  option.on_enter = [this] { OnWelcomeEnter(); };
  Component const menu = ftxui::Menu(option);

  Component const body = ftxui::Renderer(menu, [menu] {
    return ftxui::vbox({
        ftxui::text("Install a bootc system with guided disk, identity and "
                    "security setup.") |
            ftxui::dim,
        ftxui::text(""),
        menu->Render(),
    });
  });
  return ftxui::CatchEvent(body, [this](const Event& event) {
    if (event != Event::Escape) {
      return false;
    }
    SetTrap(Trap::kQuit);
    return true;
  });
}

void Installer::Impl::OnWelcomeEnter() {
  switch (welcome_index_) {
    case 0:
      ResetFlow();
      break;
    case 1:
      SetTrap(Trap::kShell);
      break;
    default:
      OpenModal(ModalKind::kRebootConfirm);
      break;
  }
}

void Installer::Impl::ResetFlow() {
  drives_.clear();
  drive_rows_.clear();
  drive_index_ = 0;
  firmware_.reset();
  validation_error_.clear();
  install_error_.clear();
  install_notice_.clear();
  log_scroll_ = 0;
  step_states_.clear();
  step_fractions_.clear();
  current_step_ = 0;
  install_state_ = InstallState::kRunning;
  install_committed_ = false;
  reboot_automatic_ = false;
  zone_filter_.clear();
  RefilterZones();
  Goto(Route::kTarget);
  RefreshDrives();
}

Component Installer::Impl::BuildTarget() {
  ftxui::MenuOption option = ftxui::MenuOption::Vertical();
  option.entries = &drive_rows_;
  option.selected = &drive_index_;
  option.on_enter = [this] { OnDriveEnter(); };
  Component const menu = ftxui::Menu(option);

  Component const body = ftxui::Renderer(menu, [this, menu] {
    if (scanning_) {
      return ftxui::text("Scanning for target disks...") | ftxui::bold;
    }
    if (drive_rows_.empty()) {
      Element message =
          drive_error_.empty()
              ? ftxui::text("No target disks found. Press Ctrl-R to rescan.")
              : ftxui::text(
                    fmt::format("Disk scan failed: {}. Press Ctrl-R to retry.",
                                drive_error_));
      return std::move(message) | ftxui::bold | ftxui::color(ftxui::Color::Red);
    }
    Elements rows{
        ftxui::text("Busy disks are listed but cannot be installed to.") |
            ftxui::dim,
        ftxui::text(""),
        menu->Render() | ftxui::vscroll_indicator | ftxui::frame | ftxui::flex,
    };
    if (!target_notice_.empty()) {
      rows.push_back(ftxui::text(""));
      rows.push_back(ftxui::text(target_notice_) | ftxui::bold |
                     ftxui::color(ftxui::Color::Red));
    }
    return ftxui::vbox(std::move(rows));
  });

  return ftxui::CatchEvent(body, [this](const Event& event) {
    if (event == Event::CtrlR) {
      RefreshDrives();
      return true;
    }
    if (event == Event::Escape) {
      Goto(Route::kWelcome);
      return true;
    }
    return false;
  });
}

void Installer::Impl::RefreshDrives() {
  if (scanning_) {
    return;
  }

  if (drive_thread_.joinable()) {
    drive_thread_.join();
  }
  scanning_ = true;
  drives_.clear();
  drive_rows_.clear();
  drive_index_ = 0;

  drive_thread_ = std::thread([this] {
    StatusOr<std::vector<engine::Device>> devices = engine::ListDisks();
    std::vector<DriveEntry> entries;
    std::vector<std::string> rows;

    std::string error =
        devices.ok() ? "" : std::string(devices.status().message());
    if (devices.ok()) {
      for (const engine::Device& device : *devices) {
        const std::string size = engine::HumanSize(device.size);
        const std::string model =
            device.model.empty() ? "(unknown)" : device.model;
        const std::string bus =
            device.transport.empty() ? "-" : device.transport;
        entries.push_back(DriveEntry{
            .path = device.path,
            .label =
                fmt::format("{} - {}, {}", device.path.string(), model, size),
            .busy = device.busy_reason.has_value(),
            .busy_reason =
                device.busy_reason.has_value()
                    ? std::string(engine::BusyReasonText(*device.busy_reason))
                    : ""});
        rows.push_back(fmt::format(
            "{:<14} {:>10}  {:<6} {:<28} {}", device.path.string(), size, bus,
            model,
            entries.back().busy
                ? fmt::format("BUSY ({})", entries.back().busy_reason)
                : "available"));
      }
    }
    Post([this, entries = std::move(entries), rows = std::move(rows),
          error = std::move(error)]() mutable {
      drives_ = std::move(entries);
      drive_rows_ = std::move(rows);
      drive_error_ = std::move(error);
      target_notice_.clear();
      drive_index_ = 0;
      scanning_ = false;
    });
  });
}

void Installer::Impl::OnDriveEnter() {
  if (drive_index_ < 0 || static_cast<size_t>(drive_index_) >= drives_.size()) {
    return;
  }
  const DriveEntry& drive = drives_[static_cast<size_t>(drive_index_)];
  if (drive.busy) {
    target_notice_ = fmt::format("{} cannot be selected: {}",
                                 drive.path.string(), drive.busy_reason);
    return;
  }
  target_notice_.clear();
  context_.drive = drive.path;
  context_.drive_label = drive.label;
  OpenModal(ModalKind::kWipe);
}

Component Installer::Impl::BuildIdentity() {
  language_entries_.assign(kLocales.size(), "");
  for (size_t index = 0; index < kLocales.size(); ++index) {
    language_entries_[index] = std::string(kLocales[index].label);
  }
  shell_entries_.assign(kShells.begin(), kShells.end());

  const Component language =
      ftxui::Toggle(&language_entries_, &language_index_);
  const Component shell = ftxui::Toggle(&shell_entries_, &shell_index_);

  auto make_input = [this](std::string* value, size_t max_bytes, bool secret) {
    ftxui::InputOption option;
    option.content = value;
    option.multiline = false;
    option.on_change = [value, max_bytes] { Truncate(value, max_bytes); };
    option.on_enter = [this] { OnIdentitySubmit(); };
    if (secret) {
      option.password = &hide_password_;
    }
    return ftxui::Input(option);
  };

  Component const hostname = make_input(&hostname_, kHostnameMaxBytes, false);
  Component const username = make_input(&username_, kPosixNameMaxBytes, false);
  Component const password = make_input(&password_, std::string::npos, true);
  Component const repeated =
      make_input(&repeated_password_, std::string::npos, true);

  ftxui::CheckboxOption reveal_option = ftxui::CheckboxOption::Simple();
  reveal_option.label = "Show password";
  reveal_option.checked = &reveal_password_;
  reveal_option.on_change = [this] { hide_password_ = !reveal_password_; };

  reveal_option.transform = [](const ftxui::EntryState& state) {
    Element const label =
        ftxui::text((state.state ? "[x] " : "[ ] ") + state.label);
    return state.active ? label | ftxui::inverted : label;
  };
  Component const reveal = ftxui::Checkbox(reveal_option);

  Component const submit = ftxui::Button(
      "[ Continue ]", [this] { OnIdentitySubmit(); },
      ftxui::ButtonOption::Ascii());

  Component const form = ftxui::Container::Vertical(Components{
      language, hostname, username, password, repeated, reveal, shell, submit});

  Component const body =
      ftxui::Renderer(form, [this, language, hostname, username, password,
                             repeated, reveal, shell, submit] {
        Elements rows{
            Field("Language", language->Render()),
            Field("Hostname", hostname->Render() | ftxui::underlined),
            Field("User name", username->Render() | ftxui::underlined),
            Field("Password", password->Render() | ftxui::underlined),
            Field("Repeat password", repeated->Render() | ftxui::underlined),
            Field("", reveal->Render()),
            Field("Shell", shell->Render()),
            ftxui::text(""),
            submit->Render(),
        };
        if (!validation_error_.empty()) {
          rows.push_back(ftxui::text(""));
          rows.push_back(ftxui::text(validation_error_) | ftxui::bold |
                         ftxui::color(ftxui::Color::Red));
        }

        rows.push_back(ftxui::text(""));
        rows.push_back(ftxui::text(fmt::format(
                           "Keyboard layout: {} (from the configuration file)",
                           context_.config.identity.keymap.Value())) |
                       ftxui::dim);
        return ftxui::vbox(std::move(rows));
      });

  return ftxui::CatchEvent(body, [this](const Event& event) {
    if (event == Event::Return) {
      OnIdentitySubmit();
      return true;
    }
    if (event != Event::Escape) {
      return false;
    }
    Goto(Route::kTarget);
    return true;
  });
}

void Installer::Impl::OnIdentitySubmit() {
  const StatusOr<Hostname> hostname = Hostname::Parse(engine::Trim(hostname_));
  if (!hostname.ok()) {
    validation_error_ = std::string(hostname.status().message());
    return;
  }
  const std::string_view username = engine::Trim(username_);
  if (username.empty()) {
    validation_error_ = "User name must not be empty";
    return;
  }
  const StatusOr<Username> parsed_username = Username::Parse(username);
  if (!parsed_username.ok()) {
    validation_error_ = std::string(parsed_username.status().message());
    return;
  }
  if (password_.empty()) {
    validation_error_ = "Password must not be empty";
    return;
  }

  if (password_.find_first_of("\r\n") != std::string::npos) {
    validation_error_ = "Password must not contain line breaks";
    return;
  }
  if (password_ != repeated_password_) {
    validation_error_ = "Passwords do not match";
    return;
  }

  validation_error_.clear();
  context_.config.identity.hostname = *hostname;
  const StatusOr<SingleLine> locale =
      SingleLine::Parse(kLocales[static_cast<size_t>(language_index_)].name);
  if (locale.ok()) {
    context_.config.identity.locale = *locale;
  }
  context_.username = *parsed_username;
  context_.user_password.Assign(password_);
  context_.user_shell = shell_index_ == 1 ? UserShell::kZsh : UserShell::kBash;
  Goto(Route::kTimezone);
}

Component Installer::Impl::BuildTimezone() {
  ftxui::InputOption filter_option;
  filter_option.content = &zone_filter_;
  filter_option.placeholder = "type to filter";
  filter_option.multiline = false;
  filter_option.on_change = [this] { RefilterZones(); };
  filter_option.on_enter = [this] { OnTimezoneEnter(); };
  Component const filter = ftxui::Input(filter_option);

  ftxui::MenuOption menu_option = ftxui::MenuOption::Vertical();
  menu_option.entries = &zone_rows_;
  menu_option.selected = &zone_index_;
  menu_option.on_enter = [this] { OnTimezoneEnter(); };
  Component const menu = ftxui::Menu(menu_option);

  Component const container =
      ftxui::Container::Vertical(Components{filter, menu});
  Component const body = ftxui::Renderer(container, [this, filter, menu] {
    return ftxui::vbox({
        Field("Filter", filter->Render() | ftxui::underlined),
        ftxui::text(fmt::format("{} zones", zone_rows_.size())) | ftxui::dim,
        ftxui::separator(),
        menu->Render() | ftxui::vscroll_indicator | ftxui::frame | ftxui::flex,
    });
  });

  return ftxui::CatchEvent(body, [this](const Event& event) {
    if (event != Event::Escape) {
      return false;
    }
    Goto(Route::kIdentity);
    return true;
  });
}

void Installer::Impl::RefilterZones() {
  const std::string needle = ToLower(zone_filter_);
  const std::string selected =
      (zone_index_ >= 0 && static_cast<size_t>(zone_index_) < zone_rows_.size())
          ? zone_rows_[static_cast<size_t>(zone_index_)]
          : context_.config.identity.timezone.Value();

  zone_rows_.clear();
  const auto& zones = Timezones();
  const auto& lowered = LoweredTimezones();
  for (size_t index = 0; index < zones.size(); ++index) {
    if (needle.empty() || lowered[index].find(needle) != std::string::npos) {
      zone_rows_.push_back(zones[index]);
    }
  }

  const auto found = std::ranges::find(zone_rows_, selected);
  zone_index_ = found != zone_rows_.end()
                    ? static_cast<int>(found - zone_rows_.begin())
                    : 0;
}

void Installer::Impl::OnTimezoneEnter() {
  if (zone_index_ < 0 ||
      static_cast<size_t>(zone_index_) >= zone_rows_.size()) {
    return;
  }
  const StatusOr<Timezone> zone =
      Timezone::Parse(zone_rows_[static_cast<size_t>(zone_index_)]);
  if (!zone.ok()) {
    return;
  }
  context_.config.identity.timezone = *zone;

  firmware_ = engine::DetectSetupMode();
  Goto(Route::kSecureBoot);
}

bool Installer::Impl::SetupModeOn() const {
  return firmware_.has_value() &&
         firmware_->state == engine::SetupMode::State::kOn;
}

Component Installer::Impl::BuildSecureBoot() {
  const ftxui::ButtonOption option = ftxui::ButtonOption::Ascii();
  Component const back =
      ftxui::Button("[ Back ]", [this] { Goto(Route::kTimezone); }, option);
  Component const enable = ftxui::Button(
      "[ Enable Secure Boot ]", [this] { OnEnableSecureBoot(); }, option);
  Component const skip = ftxui::Button(
                             "[ Continue without Secure Boot ]",
                             [this] {
                               context_.secure_boot =
                                   engine::SecureBootMode::kSkip;
                               Goto(Route::kReview);
                             },
                             option) |
                         ftxui::Maybe([this] { return !SetupModeOn(); });
  Component const reboot = ftxui::Button(
                               "[ Reboot to firmware settings ]",
                               [this] { SetTrap(Trap::kReboot); }, option) |
                           ftxui::Maybe([this] { return !SetupModeOn(); });

  Component const menu =
      ftxui::Container::Vertical(Components{back, enable, skip, reboot});
  Component const body = ftxui::Renderer(menu, [this, menu] {
    Elements rows;
    if (SetupModeOn()) {
      rows.push_back(
          ftxui::text(fmt::format("{} can enrol its own Secure Boot keys.",
                                  context_.config.branding.name.Value())));
      rows.push_back(
          ftxui::text("This writes firmware Secure Boot variables.") |
          ftxui::bold | ftxui::color(ftxui::Color::Yellow));
    } else {
      const bool off = firmware_.has_value() &&
                       firmware_->state == engine::SetupMode::State::kOff;
      rows.push_back(
          ftxui::text(off ? "Setup Mode is off; keys cannot be enrolled."
                          : "Firmware Setup Mode could not be detected.") |
          ftxui::bold | ftxui::color(ftxui::Color::Yellow));
      if (firmware_.has_value() && !firmware_->reason.empty()) {
        rows.push_back(ftxui::text(firmware_->reason) |
                       ftxui::color(ftxui::Color::Red));
      }
    }
    rows.push_back(ftxui::text(""));
    rows.push_back(menu->Render());
    return ftxui::vbox(std::move(rows));
  });

  return ftxui::CatchEvent(body, [this](const Event& event) {
    if (event != Event::Escape) {
      return false;
    }
    Goto(Route::kTimezone);
    return true;
  });
}

void Installer::Impl::OnEnableSecureBoot() {
  if (!SetupModeOn()) {
    OpenModal(ModalKind::kSetupMode);
    return;
  }
  context_.secure_boot = engine::SecureBootMode::kEnroll;
  Goto(Route::kReview);
}

Component Installer::Impl::BuildReview() {
  const ftxui::ButtonOption option = ftxui::ButtonOption::Ascii();
  Component const back =
      ftxui::Button("[ Back ]", [this] { Goto(Route::kSecureBoot); }, option);
  Component const start = ftxui::Button(
      "[ Start installation ]",
      [this] {
        Goto(Route::kInstall);
        StartInstall();
      },
      option);

  Component const menu = ftxui::Container::Vertical(Components{back, start});
  Component const body = ftxui::Renderer(menu, [this, menu] {
    return ftxui::vbox({
        ftxui::text("Review these settings carefully. Starting the "
                    "installation erases the target disk.") |
            ftxui::bold | ftxui::color(ftxui::Color::Yellow),
        ftxui::text(""),
        Field("Target disk", ftxui::text(context_.drive_label.value_or("-"))),
        Field("Hostname",
              ftxui::text(context_.config.identity.hostname.Value())),
        Field("Time zone",
              ftxui::text(context_.config.identity.timezone.Value())),
        Field("Secure Boot", ftxui::text(context_.secure_boot ==
                                                 engine::SecureBootMode::kEnroll
                                             ? "Enabled"
                                             : "Disabled")),
        ftxui::text(""),
        menu->Render(),
    });
  });

  return ftxui::CatchEvent(body, [this](const Event& event) {
    if (event != Event::Escape) {
      return false;
    }
    Goto(Route::kSecureBoot);
    return true;
  });
}

Component Installer::Impl::BuildInstall() {
  Component const body = ftxui::Renderer([this] { return RenderInstall(); });
  return ftxui::CatchEvent(body, [this](const Event& event) {
    const int page = LogPageLines();
    if (event == Event::PageUp) {
      const size_t count = SnapshotLogLines().size();
      log_scroll_ = std::min(log_scroll_ + static_cast<size_t>(page),
                             count == 0 ? 0 : count - 1);
      return true;
    }
    if (event == Event::PageDown) {
      log_scroll_ -= std::min(log_scroll_, static_cast<size_t>(page));
      return true;
    }
    if (event == Event::Escape) {
      if (install_state_ != InstallState::kRunning) {
        Goto(Route::kWelcome);
        return true;
      }
      if (install_committed_) {
        install_notice_ =
            "The target drive has been written; the installation can no "
            "longer be cancelled.";
        return true;
      }
      OpenModal(ModalKind::kCancel);
      return true;
    }
    if (event == Event::Return && install_state_ == InstallState::kFailed) {
      Goto(Route::kWelcome);
      return true;
    }
    return false;
  });
}

Element Installer::Impl::RenderSteps() const {
  Elements rows;
  size_t index = 0;
  for (const engine::Step& step : engine::Steps()) {
    const StepState state =
        index < step_states_.size() ? step_states_[index] : StepState::kPending;
    Element row = ftxui::text(fmt::format("{} {:02} {}", StepMarker(state),
                                          index + 1, step.title)) |
                  ftxui::color(StepColor(state));
    if (index == current_step_) {
      row = row | ftxui::focus;
    }
    rows.push_back(std::move(row));
    ++index;
  }
  return ftxui::vbox(std::move(rows)) | ftxui::vscroll_indicator | ftxui::frame;
}

Element Installer::Impl::RenderLog() const {
  const std::vector<std::string> lines = SnapshotLogLines();
  if (lines.empty()) {
    return ftxui::text("Waiting for the installer to start...") | ftxui::dim;
  }
  const size_t last = lines.size() - 1;
  const size_t anchor = last - std::min(log_scroll_, last);
  const int page = LogPageLines();
  const size_t start = anchor >= static_cast<size_t>(page)
                           ? anchor - static_cast<size_t>(page) + 1
                           : 0;
  const size_t end = std::min(start + static_cast<size_t>(page), lines.size());
  Elements rows;
  rows.reserve(end - start);
  for (size_t index = start; index < end; ++index) {
    Element line = ftxui::text(lines[index]) | ftxui::dim;

    if (index == anchor) {
      line = line | ftxui::focus;
    }
    rows.push_back(std::move(line));
  }
  return ftxui::vbox(std::move(rows)) | ftxui::vscroll_indicator | ftxui::frame;
}

Element Installer::Impl::RenderInstall() const {
  const std::span<const engine::Step> steps = engine::Steps();
  const std::string current = current_step_ < steps.size()
                                  ? std::string(steps[current_step_].title)
                                  : std::string("-");
  std::string status;
  switch (install_state_) {
    case InstallState::kRunning:
      status = install_committed_
                   ? "Writing to the target drive; cancellation is disabled."
                   : "Esc cancels while the drive is still untouched.";
      break;
    case InstallState::kSucceeded:
      status = "Installation completed.";
      break;
    case InstallState::kFailed:
      status = fmt::format("Installation failed: {}", install_error_);
      break;
  }

  Elements header{
      ftxui::hbox({
          ftxui::text(fmt::format("Step {}/{}  ",
                                  std::min(current_step_ + 1, steps.size()),
                                  steps.size())),
          ftxui::text(current) | ftxui::bold,
          ftxui::filler(),
          ftxui::text(fmt::format("{:02}:{:02}", install_elapsed_.count() / 60,
                                  install_elapsed_.count() % 60)),
      }),
      ftxui::gauge(static_cast<float>(OverallProgress())),
      ftxui::text(status) | ftxui::color(install_state_ == InstallState::kFailed
                                             ? ftxui::Color::Red
                                             : ftxui::Color::Cyan),
  };
  if (!install_notice_.empty()) {
    header.push_back(ftxui::text(install_notice_) | ftxui::bold |
                     ftxui::color(ftxui::Color::Yellow));
  }

  return ftxui::vbox({
      ftxui::vbox(std::move(header)),
      ftxui::separator(),
      ftxui::hbox({
          ftxui::vbox({
              ftxui::text("Installation steps") | ftxui::bold,
              RenderSteps() | ftxui::flex,
          }) | ftxui::size(ftxui::WIDTH, ftxui::EQUAL, 38),
          ftxui::separator(),
          ftxui::vbox({
              ftxui::text("Live installation log") | ftxui::bold,
              RenderLog() | ftxui::flex,
          }) | ftxui::flex,
      }) | ftxui::flex,
  });
}

Component Installer::Impl::BuildReboot() {
  const ftxui::ButtonOption option = ftxui::ButtonOption::Ascii();
  Component const shell = ftxui::Button(
      "[ Open shell ]", [this] { SetTrap(Trap::kShell); }, option);
  Component const now = ftxui::Button(
      "[ Reboot now ]", [this] { SetTrap(Trap::kReboot); }, option);

  Component const menu =
      ftxui::Container::Vertical(Components{shell, now}, &reboot_index_);
  Component const body = ftxui::Renderer(menu, [this, menu] {
    return ftxui::vbox({
        Field("Target", ftxui::text(context_.drive_label.value_or("-"))),
        ftxui::text(""),
        ftxui::text(reboot_automatic_
                        ? fmt::format("Automatic reboot in {} seconds",
                                      reboot_remaining_)
                        : "Automatic reboot cancelled.") |
            ftxui::bold | ftxui::color(ftxui::Color::Cyan),
        ftxui::text(""),
        menu->Render(),
    });
  });

  return ftxui::CatchEvent(body, [this](const Event& event) {
    if (event.is_character() || event.is_mouse() || event == Event::ArrowUp ||
        event == Event::ArrowDown || event == Event::Tab ||
        event == Event::Return || event == Event::Escape) {
      reboot_automatic_ = false;
    }
    return false;
  });
}

Component Installer::Impl::BuildModal() {
  const ftxui::ButtonOption option = ftxui::ButtonOption::Ascii();
  Component const dismiss = ftxui::Button(
      &modal_dismiss_label_, [this] { CloseModal(false); }, option);
  Component const confirm = ftxui::Button(
      &modal_confirm_label_, [this] { CloseModal(true); }, option);
  Component const buttons =
      ftxui::Container::Horizontal(Components{dismiss, confirm});

  Component const body = ftxui::Renderer(buttons, [this, buttons] {
    return ftxui::vbox({
               ftxui::text(modal_title_) | ftxui::bold,
               ftxui::separator(),
               ftxui::paragraph(modal_body_),
               ftxui::separator(),
               ftxui::hbox(
                   {ftxui::filler(), buttons->Render(), ftxui::filler()}),
           }) |
           ftxui::border | ftxui::size(ftxui::WIDTH, ftxui::LESS_THAN, 64) |
           ftxui::bgcolor(ftxui::Color::Black);
  });

  return ftxui::CatchEvent(body, [this](const Event& event) {
    if (event != Event::Escape) {
      return false;
    }
    CloseModal(false);
    return true;
  });
}

struct ModalSpec {
  std::string_view title;
  std::string_view body;
  std::string_view dismiss_label;
  std::string_view confirm_label;
};

constexpr std::array<ModalSpec, 5> kModalSpecs = {{
    {.title = "Wipe target disk?",
     .body = "All data on this disk will be destroyed.",
     .dismiss_label = "No",
     .confirm_label = "Yes"},
    {.title = "Reboot now?",
     .body = "The machine will reboot immediately.",
     .dismiss_label = "No",
     .confirm_label = "Yes"},
    {.title = "Enable Secure Boot?",
     .body = "Custom keys can only be enrolled while firmware Setup Mode is "
             "on. Reboot into the firmware settings and enable Setup Mode, "
             "typically by clearing the Secure Boot keys, then run the "
             "installer again.",
     .dismiss_label = "Back",
     .confirm_label = "Reboot"},
    {.title = "Installation failed",
     .body = "Unknown error",
     .dismiss_label = "View log",
     .confirm_label = "Back to start"},
    {.title = "Cancel installation?",
     .body = "The installer will stop and clean up its mounts.",
     .dismiss_label = "No",
     .confirm_label = "Yes"},
}};

void Installer::Impl::OpenModal(ModalKind kind) {
  if (modal_open_) {
    pending_modal_ = kind;
    return;
  }
  const ModalSpec& spec = kModalSpecs[static_cast<size_t>(kind)];
  modal_kind_ = kind;
  modal_title_ = std::string(spec.title);
  modal_dismiss_label_ = std::string(spec.dismiss_label);
  modal_confirm_label_ = std::string(spec.confirm_label);
  modal_body_ = kind == ModalKind::kFailure && !install_error_.empty()
                    ? install_error_
                    : std::string(spec.body);
  modal_open_ = true;
}

void Installer::Impl::CloseModal(bool confirmed) {
  modal_open_ = false;
  switch (modal_kind_) {
    case ModalKind::kWipe:
      if (confirmed) {
        Goto(Route::kIdentity);
      }
      break;
    case ModalKind::kRebootConfirm:
    case ModalKind::kSetupMode:
      if (confirmed) {
        SetTrap(Trap::kReboot);
      }
      break;
    case ModalKind::kFailure:
      if (confirmed) {
        Goto(Route::kWelcome);
      }
      break;
    case ModalKind::kCancel:

      if (confirmed && !install_committed_) {
        install_cancel_ = true;

        install_notice_ = "Cancelling; unmounting and cleaning up...";
      }
      break;
  }
  if (pending_modal_.has_value()) {
    const ModalKind next = *pending_modal_;
    pending_modal_.reset();
    OpenModal(next);
  }
}

void Installer::Impl::StartInstall() {
  if (context_.k3s_token.Empty() && context_.k3s_token_file.has_value()) {
    StatusOr<SecretString> token =
        engine::ReadFirstSecretLine(*context_.k3s_token_file);
    if (!token.ok()) {
      install_state_ = InstallState::kFailed;
      install_error_ = std::string(token.status().message());
      OpenModal(ModalKind::kFailure);
      return;
    }
    context_.k3s_token = std::move(*token);
  }
  const std::span<const engine::Step> steps = engine::Steps();
  install_state_ = InstallState::kRunning;
  install_committed_ = false;
  install_cancel_ = false;
  install_error_.clear();
  install_notice_.clear();
  log_scroll_ = 0;
  current_step_ = 0;
  step_states_.assign(steps.size(), StepState::kPending);
  step_fractions_.assign(steps.size(), 0.0);
  install_started_ = std::chrono::steady_clock::now();
  install_elapsed_ = std::chrono::seconds{0};
  {
    const std::lock_guard<std::mutex> lock(commit_mutex_);
    commit_acknowledged_ = false;
  }

  engine::Context worker_context = context_.Clone();

  worker_context.parts = engine::DerivedPartitions{};

  if (install_thread_.joinable()) {
    install_thread_.join();
  }
  install_thread_ =
      std::thread([this, worker_context = std::move(worker_context)]() mutable {
        RunInstall(std::move(worker_context));
      });

  WipeString(&password_);
  WipeString(&repeated_password_);
  reveal_password_ = false;
  hide_password_ = true;
}

void Installer::Impl::RunInstall(engine::Context context) {
  Status status;
  try {
    engine::Runner runner(std::move(context));
    runner.SetCancelCallback([this] { return install_cancel_.load(); });
    runner.SetProgressCallback([this](size_t index, double fraction) {
      Post([this, index, fraction] { OnProgress(index, fraction); });
    });
    runner.SetSkipCallback([this](size_t index) {
      Post([this, index] { OnStepSkipped(index); });
    });
    runner.SetCommitCallback(
        [this](size_t index) { return AwaitCommit(index); });

    auto logger = spdlog::get(std::string(engine::kInstallationLogTarget));
    if (logger != nullptr) {
      logger->info("installation worker started");
    }

    status = runner.Execute(engine::Steps());
    if (logger != nullptr) {
      if (status.ok()) {
        logger->info("installation completed");
      } else {
        logger->error("installation failed: {}", status.message());
      }
    }
  } catch (const std::exception& error) {
    status = InternalError("installation worker crashed: {}", error.what());
  }
  Post([this, status] { OnInstallFinished(status); });
}

bool Installer::Impl::AwaitCommit(size_t step_index) {
  std::unique_lock<std::mutex> lock(commit_mutex_);
  commit_acknowledged_ = false;
  Post([this, step_index] {
    {
      const std::lock_guard<std::mutex> ui_lock(commit_mutex_);
      if (ui_finished_ || install_cancel_.load()) {
        commit_signal_.notify_all();
        return;
      }
      commit_acknowledged_ = true;
    }
    install_committed_ = true;
    current_step_ = step_index;
    commit_signal_.notify_all();
  });

  commit_signal_.wait(lock, [this] {
    return commit_acknowledged_ || ui_finished_ || install_cancel_.load();
  });
  return commit_acknowledged_ && !ui_finished_ && !install_cancel_.load();
}

void Installer::Impl::OnProgress(size_t step_index, double fraction) {
  if (step_index >= step_states_.size()) {
    return;
  }
  current_step_ = step_index;

  for (size_t index = 0; index < step_index; ++index) {
    if (step_states_[index] == StepState::kActive ||
        step_states_[index] == StepState::kPending) {
      step_states_[index] = StepState::kDone;
      step_fractions_[index] = 1.0;
    }
  }
  step_fractions_[step_index] = std::clamp(fraction, 0.0, 1.0);
  step_states_[step_index] =
      fraction >= 1.0 ? StepState::kDone : StepState::kActive;
}

void Installer::Impl::OnStepSkipped(size_t step_index) {
  if (step_index >= step_states_.size()) {
    return;
  }
  step_states_[step_index] = StepState::kSkipped;
  step_fractions_[step_index] = 1.0;
}

void Installer::Impl::OnInstallFinished(const Status& status) {
  install_elapsed_ = std::chrono::duration_cast<std::chrono::seconds>(
      std::chrono::steady_clock::now() - install_started_);

  install_notice_.clear();
  if (status.ok()) {
    install_state_ = InstallState::kSucceeded;
    for (size_t index = 0; index < step_states_.size(); ++index) {
      if (step_states_[index] != StepState::kSkipped) {
        step_states_[index] = StepState::kDone;
        step_fractions_[index] = 1.0;
      }
    }

    context_.user_password.Clear();
    context_.k3s_token.Clear();
    reboot_automatic_ = true;
    reboot_remaining_ = kRebootDelaySeconds;
    reboot_deadline_ = std::chrono::steady_clock::now() +
                       std::chrono::seconds{kRebootDelaySeconds};
    Goto(Route::kReboot);
    return;
  }

  install_state_ = InstallState::kFailed;
  install_error_ = FrameSafe(std::string(status.message()));
  context_.user_password = SecretString{};
  context_.k3s_token = SecretString{};
  WipeString(&password_);
  WipeString(&repeated_password_);
  if (current_step_ < step_states_.size()) {
    step_states_[current_step_] = StepState::kFailed;
  }

  OpenModal(ModalKind::kFailure);
}

double Installer::Impl::OverallProgress() const {
  if (install_state_ == InstallState::kSucceeded) {
    return 1.0;
  }
  double total = 0.0;
  double progressed = 0.0;
  size_t index = 0;
  for (const engine::Step& step : engine::Steps()) {
    const double weight = step.weight;
    total += weight;
    if (index < step_fractions_.size()) {
      progressed += weight * step_fractions_[index];
    }
    ++index;
  }
  return total > 0.0 ? progressed / total : 0.0;
}

Element Installer::Impl::RenderChrome(Element body) const {
  const ftxui::ScreenInteractive* screen = ftxui::ScreenInteractive::Active();
  if (screen != nullptr &&
      (screen->dimx() < kMinColumns || screen->dimy() < kMinRows)) {
    return ftxui::vbox({
               ftxui::filler(),
               ftxui::text(
                   fmt::format("Terminal too small: {}x{} is the minimum.",
                               kMinColumns, kMinRows)) |
                   ftxui::bold | ftxui::center,
               ftxui::text("Please enlarge the terminal window.") |
                   ftxui::center | ftxui::dim,
               ftxui::filler(),
           }) |
           ftxui::border;
  }
  return ftxui::vbox({
             ftxui::hbox({
                 ftxui::text(Title()) | ftxui::bold,
                 ftxui::filler(),
                 ftxui::text(std::string(Subtitle(CurrentRoute()))) |
                     ftxui::dim,
             }),
             ftxui::separator(),
             std::move(body) | ftxui::flex,
             ftxui::separator(),
             ftxui::text(std::string(Footer(CurrentRoute()))) | ftxui::dim,
         }) |
         ftxui::border;
}

std::string Installer::Impl::Title() const {
  const std::string& brand = context_.config.branding.name.Value();
  switch (CurrentRoute()) {
    case Route::kWelcome:
      return context_.image_version.has_value()
                 ? fmt::format("{} {} Installer", brand,
                               *context_.image_version)
                 : fmt::format("{} Installer", brand);
    case Route::kTarget:
      return "Select Target Drive";
    case Route::kIdentity:
      return "System Identity";
    case Route::kTimezone:
      return "Select Time Zone";
    case Route::kSecureBoot:
      return "Secure Boot";
    case Route::kReview:
      return "Does this look right?";
    case Route::kInstall:
      return fmt::format("Installing {}", brand);
    case Route::kReboot:
      return "Install Complete";
  }
  return brand;
}

void Installer::Impl::OnTick() {
  if (CurrentRoute() == Route::kInstall &&
      install_state_ == InstallState::kRunning) {
    install_elapsed_ = std::chrono::duration_cast<std::chrono::seconds>(
        std::chrono::steady_clock::now() - install_started_);
  }
  if (CurrentRoute() == Route::kReboot && reboot_automatic_) {
    const auto now = std::chrono::steady_clock::now();
    if (now >= reboot_deadline_) {
      SetTrap(Trap::kReboot);
      return;
    }
    reboot_remaining_ =
        static_cast<int>(std::chrono::duration_cast<std::chrono::seconds>(
                             reboot_deadline_ - now)
                             .count()) +
        1;
  }
}

void Installer::Impl::SetTrap(Trap trap) {
  trap_ = trap;

  ticker_stop_ = true;
  if (ticker_thread_.joinable()) {
    ticker_thread_.join();
  }
  const std::lock_guard<std::mutex> lock(screen_mutex_);
  if (screen_ != nullptr) {
    screen_->Exit();
  }
}

void Installer::Impl::Post(std::function<void()> task) {
  const std::lock_guard<std::mutex> lock(screen_mutex_);
  if (screen_ != nullptr) {
    screen_->Post(std::move(task));
  }
}

void Installer::Impl::StartTicker() {
  ticker_stop_ = false;
  ticker_thread_ = std::thread([this] {
    while (!ticker_stop_.load()) {
      std::this_thread::sleep_for(kTickInterval);
      if (ticker_stop_.load()) {
        break;
      }

      if (!ticking_.load()) {
        continue;
      }
      Post([this] { OnTick(); });
    }
  });
}

void Installer::Impl::StopWorkers() {
  ticker_stop_ = true;
  ticking_ = false;
  if (ticker_thread_.joinable()) {
    ticker_thread_.join();
  }

  install_cancel_ = true;
  {
    const std::lock_guard<std::mutex> lock(commit_mutex_);
    ui_finished_ = true;
  }
  commit_signal_.notify_all();
  if (install_thread_.joinable()) {
    if (install_state_ == InstallState::kRunning) {
      std::fputs("Waiting for the installer to clean up its mounts...\n",
                 stderr);
      std::fflush(stderr);
    }
    install_thread_.join();
  }
  if (drive_thread_.joinable()) {
    drive_thread_.join();
  }
}

Installer::Installer(engine::Context context)
    : impl_(std::make_unique<Impl>(std::move(context))) {}

Installer::~Installer() = default;

Installer::Installer(Installer&&) noexcept = default;
Installer& Installer::operator=(Installer&&) noexcept = default;

Trap Installer::Run() { return impl_->Run(); }

engine::Context Installer::IntoContext() && { return impl_->TakeContext(); }

}  // namespace puu::tui
