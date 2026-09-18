// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_TUI_THEME_H_
#define PUU_TUI_THEME_H_

#include <ftxui/dom/elements.hpp>
#include <ftxui/screen/color.hpp>

namespace puu::tui::theme {

inline ftxui::Color Accent() { return ftxui::Color::Cyan; }
inline ftxui::Color Success() { return ftxui::Color::Green; }
inline ftxui::Color Warning() { return ftxui::Color::Yellow; }
inline ftxui::Color Danger() { return ftxui::Color::Red; }
inline ftxui::Color Muted() { return ftxui::Color::GrayDark; }

inline ftxui::Decorator Brand() { return ftxui::bold | ftxui::color(Accent()); }

inline ftxui::Decorator Heading() { return ftxui::bold; }

inline ftxui::Decorator Hint() { return ftxui::dim; }

inline ftxui::Decorator Alert() { return ftxui::bold | ftxui::color(Danger()); }

inline ftxui::Decorator Caution() {
  return ftxui::bold | ftxui::color(Warning());
}

inline ftxui::Decorator Disabled() {
  return ftxui::dim | ftxui::color(Muted());
}

}  // namespace puu::tui::theme

#endif  // PUU_TUI_THEME_H_
