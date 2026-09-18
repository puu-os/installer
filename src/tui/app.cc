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
#include "tui/theme.h"
#include "tui/timezone.h"
#include "tui/widgets.h"

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

// Visible rows of a selection list embedded in a form.
constexpr int kEmbeddedListRows = 4;

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

struct ShellChoice {
  std::string_view label;
  UserShell shell;
};

constexpr std::array<ShellChoice, 2> kShells{{
    {.label = "Bash", .shell = UserShell::kBash},
    {.label = "Zsh", .shell = UserShell::kZsh},
}};

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

enum class SecureBootAction : std::uint8_t {
  kBack,
  kEnroll,
  kSkip,
  kRebootFirmware,
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
  std::string id;
  uint64_t size = 0;
  std::string label;
  bool busy = false;
  std::string busy_reason;
};

struct RouteInfo {
  std::string_view subtitle;
  std::string_view footer;
};

constexpr std::array<RouteInfo, 8> kRouteInfo = {{
    {.subtitle = "Guided system installation",
     .footer = "Up/Down move   Enter select   Esc quit"},
    {.subtitle = "Choose the disk to erase and install to",
     .footer = "Up/Down move   Enter select   Ctrl-R rescan   Esc back"},
    {.subtitle = "Name the machine and create the first user",
     .footer = "Tab next field   Up/Down move   Enter continue   Esc back"},
    {.subtitle = "Set the system time zone",
     .footer = "Type to filter   Tab to list   Enter select   Esc back"},
    {.subtitle = "Decide how the firmware trusts this system",
     .footer = "Up/Down move   Enter select   Esc back"},
    {.subtitle = "Confirm before anything is written",
     .footer = "Up/Down move   Enter select   Esc back"},
    {.subtitle = "Installation in progress",
     .footer = "PgUp/PgDn scroll log   Esc cancel"},
    {.subtitle = "Installation complete",
     .footer = "Any key cancels the countdown   Enter select"},
}};

constexpr std::array<std::string_view, 5> kWizardSteps{
    "Target", "Identity", "Time zone", "Security", "Review"};

std::string_view Subtitle(Route route) {
  return kRouteInfo[static_cast<size_t>(route)].subtitle;
}

std::optional<size_t> RailIndex(Route route) {
  constexpr auto kFirst = static_cast<size_t>(Route::kTarget);
  constexpr auto kLast = static_cast<size_t>(Route::kReview);
  const auto index = static_cast<size_t>(route);
  if (index < kFirst || index > kLast) {
    return std::nullopt;
  }
  return index - kFirst;
}

std::string_view Footer(Route route) {
  return kRouteInfo[static_cast<size_t>(route)].footer;
}

// The entry of `choices` picked by a list selector, guarded against an index
// that has not caught up with a shorter list.
template <typename Choices>
const auto& Chosen(const Choices& choices, int index) {
  return choices[std::min(static_cast<size_t>(std::max(index, 0)),
                          choices.size() - 1)];
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
  [[nodiscard]] Element RenderSummary() const;
  [[nodiscard]] Element RenderInstall() const;
  [[nodiscard]] Element RenderSteps() const;
  [[nodiscard]] Element RenderLog() const;
  [[nodiscard]] std::string Title() const;

  bool OnGlobalEvent(const Event& event);
  void OnWelcomeEnter();
  void OnDriveEnter();
  void OnIdentitySubmit();
  void OnTimezoneEnter();
  void OnSecureBootEnter();
  void OnEnableSecureBoot();
  void OnTick();

  void ResetFlow();
  void RefreshDrives();
  void RefreshSecureBootRows();
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

  std::vector<MenuRow> welcome_rows_;
  int welcome_index_ = 0;

  std::vector<DriveEntry> drives_;
  std::vector<MenuRow> drive_rows_;
  std::string drive_error_;
  int drive_index_ = 0;
  bool scanning_ = false;
  std::thread drive_thread_;

  int language_index_ = 0;
  int shell_index_ = 0;
  std::vector<MenuRow> language_rows_;
  std::vector<MenuRow> shell_rows_;
  std::string hostname_;
  std::string username_;
  std::string password_;
  std::string repeated_password_;
  bool reveal_password_ = false;
  bool hide_password_ = true;
  std::string validation_error_;

  std::string zone_filter_;
  std::vector<MenuRow> zone_rows_;
  int zone_index_ = 0;

  std::optional<engine::SetupMode> firmware_;
  std::vector<MenuRow> secure_boot_rows_;
  std::vector<SecureBootAction> secure_boot_actions_;
  int secure_boot_index_ = 0;

  std::vector<MenuRow> review_rows_;
  int review_index_ = 0;

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
  std::vector<MenuRow> reboot_rows_;
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
  welcome_rows_ = {
      {.label = "Install system",
       .detail = "erase a disk and deploy the image"},
      {.label = "Open recovery shell",
       .detail = "work on this machine without installing"},
      {.label = "Reboot machine", .detail = "restart without any changes"},
  };
  Component const menu = SelectList({.rows = &welcome_rows_,
                                     .selected = &welcome_index_,
                                     .on_enter = [this] { OnWelcomeEnter(); }});

  Component const body = ftxui::Renderer(menu, [this, menu] {
    const std::string& brand = context_.config.branding.name.Value();
    return ftxui::vbox({
        ftxui::text(context_.image_version.has_value()
                        ? fmt::format("{} {}", brand, *context_.image_version)
                        : brand) |
            theme::Brand(),
        ftxui::text(fmt::format("Image: {}", context_.target_imgref.Value())) |
            theme::Hint(),
        ftxui::text(""),
        ftxui::paragraph("This installer sets up the disk layout, the first "
                         "user account, the time zone and Secure Boot, then "
                         "deploys the system image."),
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
  review_index_ = 0;
  zone_filter_.clear();
  RefilterZones();
  Goto(Route::kTarget);
  RefreshDrives();
}

Component Installer::Impl::BuildTarget() {
  Component const menu = SelectList({.rows = &drive_rows_,
                                     .selected = &drive_index_,
                                     .on_enter = [this] { OnDriveEnter(); }});

  Component const body = ftxui::Renderer(menu, [this, menu] {
    if (scanning_) {
      return ftxui::text("Scanning for target disks...") | theme::Heading();
    }
    if (drive_rows_.empty()) {
      Element message =
          drive_error_.empty()
              ? ftxui::text("No target disks found. Press Ctrl-R to rescan.")
              : ftxui::text(
                    fmt::format("Disk scan failed: {}. Press Ctrl-R to retry.",
                                drive_error_));
      return std::move(message) | theme::Alert();
    }
    return ftxui::vbox({
        ftxui::paragraph("Everything on the selected disk is erased. Disks "
                         "in use are shown but cannot be selected.") |
            theme::Hint(),
        ftxui::text(""),
        menu->Render() | ftxui::vscroll_indicator | ftxui::yframe | ftxui::flex,
    });
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
    std::vector<MenuRow> rows;

    std::string error =
        devices.ok() ? "" : std::string(devices.status().message());
    if (devices.ok()) {
      for (const engine::Device& device : *devices) {
        const std::string size = engine::HumanSize(device.size);
        const std::string model =
            device.model.empty() ? "(unknown)" : device.model;
        const std::string bus =
            device.transport.empty() ? "-" : device.transport;
        const bool busy = device.busy_reason.has_value();
        const std::string reason =
            busy ? std::string(engine::BusyReasonText(*device.busy_reason))
                 : "";
        entries.push_back(DriveEntry{
            .path = device.path,
            .id = device.id,
            .size = device.size,
            .label =
                fmt::format("{} - {}, {}", device.path.string(), model, size),
            .busy = busy,
            .busy_reason = reason});
        rows.push_back(
            MenuRow{.label = device.path.string(),
                    .detail = fmt::format("{:>10}  {:<5} {}", size, bus, model),
                    .note = busy ? fmt::format("in use: {}", reason) : "",
                    .enabled = !busy});
      }
    }
    Post([this, entries = std::move(entries), rows = std::move(rows),
          error = std::move(error)]() mutable {
      drives_ = std::move(entries);
      drive_rows_ = std::move(rows);
      drive_error_ = std::move(error);
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
    return;
  }
  context_.drive = drive.path;
  // Recorded here so the install confirms it is erasing the disk shown on the
  // review screen, not whatever later occupies this kernel name.
  context_.drive_id = drive.id;
  context_.drive_size = drive.size;
  context_.drive_label = drive.label;
  OpenModal(ModalKind::kWipe);
}

Component Installer::Impl::BuildIdentity() {
  language_rows_.clear();
  language_rows_.reserve(kLocales.size());
  for (const LocaleChoice& locale : kLocales) {
    language_rows_.push_back(MenuRow{.label = std::string(locale.label),
                                     .detail = std::string(locale.name)});
  }
  shell_rows_.clear();
  shell_rows_.reserve(kShells.size());
  for (const ShellChoice& choice : kShells) {
    shell_rows_.push_back(
        MenuRow{.label = std::string(choice.label),
                .detail = std::string(UserShellPath(choice.shell))});
  }

  Component const language =
      SelectList({.rows = &language_rows_, .selected = &language_index_});
  Component const shell =
      SelectList({.rows = &shell_rows_, .selected = &shell_index_});

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

  Component const body = ftxui::Renderer(form, [this, language, hostname,
                                                username, password, repeated,
                                                reveal, shell, submit] {
    Elements rows{
        Field("Language",
              language->Render() | ftxui::vscroll_indicator | ftxui::yframe |
                  ftxui::size(ftxui::HEIGHT, ftxui::EQUAL, kEmbeddedListRows)),
        Field("Hostname", hostname->Render() | ftxui::underlined),
        Field("User name", username->Render() | ftxui::underlined),
        Field("Password", password->Render() | ftxui::underlined),
        Field("Repeat password", repeated->Render() | ftxui::underlined),
        Field("", reveal->Render()),
        Field("Shell", shell->Render()),
        ftxui::text(""),
        submit->Render(),
    };
    Elements framed{ftxui::vbox(std::move(rows)) | ftxui::vscroll_indicator |
                    ftxui::yframe | ftxui::flex};
    if (!validation_error_.empty()) {
      framed.push_back(ftxui::text(validation_error_) | theme::Alert());
    }
    return ftxui::vbox(std::move(framed));
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
      SingleLine::Parse(Chosen(kLocales, language_index_).name);
  if (locale.ok()) {
    context_.config.identity.locale = *locale;
  }
  context_.username = *parsed_username;
  context_.user_password.Assign(password_);
  context_.user_shell = Chosen(kShells, shell_index_).shell;
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

  Component const menu = SelectList(
      {.rows = &zone_rows_, .selected = &zone_index_, .on_enter = [this] {
         OnTimezoneEnter();
       }});

  Component const container =
      ftxui::Container::Vertical(Components{filter, menu});
  Component const body = ftxui::Renderer(container, [this, filter, menu] {
    return ftxui::vbox({
        Field("Filter", filter->Render() | ftxui::underlined),
        ftxui::text(fmt::format("{} matching zones", zone_rows_.size())) |
            theme::Hint(),
        ftxui::separator(),
        menu->Render() | ftxui::vscroll_indicator | ftxui::yframe | ftxui::flex,
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
          ? zone_rows_[static_cast<size_t>(zone_index_)].label
          : context_.config.identity.timezone.Value();

  zone_rows_.clear();
  const auto& zones = Timezones();
  const auto& lowered = LoweredTimezones();
  for (size_t index = 0; index < zones.size(); ++index) {
    if (needle.empty() || lowered[index].find(needle) != std::string::npos) {
      zone_rows_.push_back(MenuRow{.label = zones[index]});
    }
  }

  const auto found = std::ranges::find_if(
      zone_rows_,
      [&selected](const MenuRow& row) { return row.label == selected; });
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
      Timezone::Parse(zone_rows_[static_cast<size_t>(zone_index_)].label);
  if (!zone.ok()) {
    return;
  }
  context_.config.identity.timezone = *zone;

  firmware_ = engine::DetectSetupMode();
  RefreshSecureBootRows();
  Goto(Route::kSecureBoot);
}

bool Installer::Impl::SetupModeOn() const {
  return firmware_.has_value() &&
         firmware_->state == engine::SetupMode::State::kOn;
}

void Installer::Impl::RefreshSecureBootRows() {
  secure_boot_rows_.clear();
  secure_boot_actions_.clear();
  const auto add = [this](SecureBootAction action, MenuRow row) {
    secure_boot_actions_.push_back(action);
    secure_boot_rows_.push_back(std::move(row));
  };
  add(SecureBootAction::kBack,
      {.label = "Back", .detail = "return to the time zone"});
  add(SecureBootAction::kEnroll, {.label = "Enable Secure Boot",
                                  .detail = "enrol this system's own keys"});
  if (!SetupModeOn()) {
    add(SecureBootAction::kSkip,
        {.label = "Continue without Secure Boot",
         .detail = "leave the firmware keys untouched"});
    add(SecureBootAction::kRebootFirmware,
        {.label = "Reboot to firmware settings",
         .detail = "clear the keys to enter Setup Mode"});
  }
  secure_boot_index_ = static_cast<int>(SecureBootAction::kEnroll);
}

void Installer::Impl::OnSecureBootEnter() {
  if (secure_boot_index_ < 0 ||
      static_cast<size_t>(secure_boot_index_) >= secure_boot_actions_.size()) {
    return;
  }
  switch (secure_boot_actions_[static_cast<size_t>(secure_boot_index_)]) {
    case SecureBootAction::kBack:
      Goto(Route::kTimezone);
      return;
    case SecureBootAction::kEnroll:
      OnEnableSecureBoot();
      return;
    case SecureBootAction::kSkip:
      context_.secure_boot = engine::SecureBootMode::kSkip;
      Goto(Route::kReview);
      return;
    case SecureBootAction::kRebootFirmware:
      SetTrap(Trap::kRebootFirmware);
      return;
  }
}

Component Installer::Impl::BuildSecureBoot() {
  Component const menu =
      SelectList({.rows = &secure_boot_rows_,
                  .selected = &secure_boot_index_,
                  .on_enter = [this] { OnSecureBootEnter(); }});
  Component const body = ftxui::Renderer(menu, [this, menu] {
    Elements rows;
    if (SetupModeOn()) {
      rows.push_back(
          ftxui::paragraph(fmt::format("{} can enrol its own Secure Boot keys.",
                                       context_.config.branding.name.Value())));
      rows.push_back(
          ftxui::paragraph("This writes firmware Secure Boot variables.") |
          theme::Caution());
    } else {
      const bool off = firmware_.has_value() &&
                       firmware_->state == engine::SetupMode::State::kOff;
      rows.push_back(
          ftxui::paragraph(off ? "Setup Mode is off; keys cannot be enrolled."
                               : "Firmware Setup Mode could not be detected.") |
          theme::Caution());
      if (firmware_.has_value() && !firmware_->reason.empty()) {
        rows.push_back(ftxui::paragraph(firmware_->reason) |
                       ftxui::color(theme::Danger()));
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

Element Installer::Impl::RenderSummary() const {
  return ftxui::vbox({
      Field("Target disk", ftxui::text(context_.drive.string())),
      Field("Disk model", ftxui::text(context_.drive_label.value_or("-"))),
      Field("Capacity", ftxui::text(engine::HumanSize(context_.drive_size))),
      Field("Hostname", ftxui::text(context_.config.identity.hostname.Value())),
      Field("User name", ftxui::text(context_.username.has_value()
                                         ? context_.username->Value()
                                         : "-")),
      Field("Shell",
            ftxui::text(std::string(UserShellPath(context_.user_shell)))),
      Field("Language", ftxui::text(fmt::format(
                            "{} ({})", Chosen(kLocales, language_index_).label,
                            context_.config.identity.locale.Value()))),
      Field("Keyboard", ftxui::text(context_.config.identity.keymap.Value())),
      Field("Time zone",
            ftxui::text(context_.config.identity.timezone.Value())),
      Field("Secure Boot",
            ftxui::text(context_.secure_boot == engine::SecureBootMode::kEnroll
                            ? "Enabled"
                            : "Disabled")),
  });
}

Component Installer::Impl::BuildReview() {
  review_rows_ = {
      {.label = "Back", .detail = "change the platform security choice"},
      {.label = "Start installation",
       .detail = "begin writing",
       .note = "erases the target disk"},
  };
  Component const menu = SelectList(
      {.rows = &review_rows_, .selected = &review_index_, .on_enter = [this] {
         if (review_index_ == 0) {
           Goto(Route::kSecureBoot);
           return;
         }
         Goto(Route::kInstall);
         StartInstall();
       }});
  Component const body = ftxui::Renderer(menu, [this, menu] {
    return ftxui::vbox({
               ftxui::paragraph("Nothing has been written yet. Starting the "
                                "installation erases the target disk.") |
                   theme::Caution(),
               ftxui::text(""),
               RenderSummary(),
               ftxui::text(""),
               menu->Render(),
           }) |
           ftxui::vscroll_indicator | ftxui::yframe;
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
  reboot_rows_ = {
      {.label = "Reboot now", .detail = "start the installed system"},
      {.label = "Open shell", .detail = "inspect the result before rebooting"},
  };
  Component const menu = SelectList(
      {.rows = &reboot_rows_, .selected = &reboot_index_, .on_enter = [this] {
         SetTrap(reboot_index_ == 0 ? Trap::kReboot : Trap::kShell);
       }});
  Component const body = ftxui::Renderer(menu, [this, menu] {
    return ftxui::vbox({
        ftxui::text(fmt::format("{} is installed on {}.",
                                context_.config.branding.name.Value(),
                                context_.drive.string())) |
            theme::Brand(),
        ftxui::text(""),
        ftxui::text(reboot_automatic_
                        ? fmt::format("Automatic reboot in {} seconds",
                                      reboot_remaining_)
                        : "Automatic reboot cancelled.") |
            ftxui::bold | ftxui::color(theme::Accent()),
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
  const std::optional<size_t> rail = RailIndex(CurrentRoute());
  Elements rows{
      ftxui::hbox({
          ftxui::text(Title()) | theme::Brand(),
          ftxui::filler(),
          ftxui::text(std::string(Subtitle(CurrentRoute()))) | theme::Hint(),
      }),
      ftxui::separator(),
  };
  if (rail.has_value()) {
    rows.push_back(StepRail(kWizardSteps, *rail));
  }
  rows.push_back(std::move(body) | ftxui::flex);
  rows.push_back(ftxui::separator());
  rows.push_back(ftxui::text(std::string(Footer(CurrentRoute()))) |
                 theme::Hint());
  return ftxui::vbox(std::move(rows)) | ftxui::border;
}

std::string Installer::Impl::Title() const {
  const std::string& brand = context_.config.branding.name.Value();
  switch (CurrentRoute()) {
    case Route::kWelcome:
      return fmt::format("{} Installer", brand);
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
    screen_->PostEvent(Event::Custom);
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
