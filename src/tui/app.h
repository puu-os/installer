// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_TUI_APP_H_
#define PUU_TUI_APP_H_

#include <memory>

#include "engine/context.h"

namespace puu::tui {

enum class Trap { kQuit, kReboot, kShell };

class Installer {
 public:
  explicit Installer(engine::Context context);
  ~Installer();
  Installer(const Installer&) = delete;
  Installer& operator=(const Installer&) = delete;
  // Movable so that main() can restart the UI after a recovery shell without
  // rebuilding the context.
  Installer(Installer&&) noexcept;
  Installer& operator=(Installer&&) noexcept;

  Trap Run();

  // Surrenders the context so a new Installer can resume after a shell.
  engine::Context IntoContext() &&;

 private:
  // The terminal, the installation worker and the screen state stay out of the
  // header: main() only ever needs the trap and the context.
  class Impl;

  std::unique_ptr<Impl> impl_;
};

}  // namespace puu::tui

#endif  // PUU_TUI_APP_H_
