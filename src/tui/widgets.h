// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_TUI_WIDGETS_H_
#define PUU_TUI_WIDGETS_H_

#include <cstddef>
#include <functional>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include <ftxui/component/component_base.hpp>
#include <ftxui/dom/elements.hpp>

namespace puu::tui {

// One row of a selection list. `detail` is dimmed and clipped when the terminal
// is narrow, `note` is highlighted, and rows with `enabled` false are rendered
// but never reachable by the cursor.
struct MenuRow {
  std::string label;
  std::string detail;
  std::string note;
  bool enabled = true;
};

struct SelectListOptions {
  // Both pointers must outlive the returned component.
  const std::vector<MenuRow>* rows = nullptr;
  int* selected = nullptr;
  std::function<void()> on_enter;
};

// A vertical list selector: the single choice widget used by every screen.
//
// Labels share one column sized to the widest entry, so details line up without
// the caller padding anything. The caller decides how the list is framed; wrap
// the rendered element in `vscroll_indicator | yframe` to bound a long list.
ftxui::Component SelectList(SelectListOptions options);

// A breadcrumb of wizard steps with the step at `current` highlighted.
ftxui::Element StepRail(std::span<const std::string_view> labels,
                        size_t current);

}  // namespace puu::tui

#endif  // PUU_TUI_WIDGETS_H_
