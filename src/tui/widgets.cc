// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "tui/widgets.h"

#include <algorithm>
#include <cstddef>
#include <memory>
#include <utility>

#include <ftxui/component/event.hpp>
#include <ftxui/screen/string.hpp>

#include "tui/theme.h"

namespace puu::tui {
namespace {

using ftxui::Component;
using ftxui::Element;
using ftxui::Elements;
using ftxui::Event;

constexpr std::string_view kCursor = "> ";
constexpr std::string_view kNoCursor = "  ";
constexpr std::string_view kRailSeparator = " > ";

class SelectListBase : public ftxui::ComponentBase {
 public:
  explicit SelectListBase(SelectListOptions options)
      : options_(std::move(options)) {}

  Element Render() override {
    const std::vector<MenuRow>& rows = *options_.rows;
    if (rows.empty()) {
      return ftxui::text("");
    }
    Normalise();
    int label_width = 0;
    for (const MenuRow& row : rows) {
      label_width = std::max(label_width, ftxui::string_width(row.label));
    }
    Elements items;
    items.reserve(rows.size());
    for (size_t index = 0; index < rows.size(); ++index) {
      items.push_back(RenderRow(rows[index], index, label_width));
    }
    return ftxui::vbox(std::move(items));
  }

  bool OnEvent(Event event) override {
    if (options_.rows->empty()) {
      return false;
    }
    Normalise();
    if (event == Event::ArrowUp) {
      return Step(-1);
    }
    if (event == Event::ArrowDown) {
      return Step(1);
    }
    if (event == Event::Home) {
      return Jump(0, 1);
    }
    if (event == Event::End) {
      return Jump(static_cast<int>(options_.rows->size()) - 1, -1);
    }
    if (event == Event::Return) {
      return Activate();
    }
    return false;
  }

  bool Focusable() const override { return true; }

 private:
  [[nodiscard]] Element RenderRow(const MenuRow& row, size_t index,
                                  int label_width) const {
    const bool current = static_cast<size_t>(*options_.selected) == index;
    Element label = ftxui::text(row.label) |
                    ftxui::size(ftxui::WIDTH, ftxui::EQUAL, label_width);
    if (!row.enabled) {
      label = std::move(label) | theme::Disabled();
    }

    Elements cells;
    cells.push_back(ftxui::text(std::string(current ? kCursor : kNoCursor)));
    cells.push_back(std::move(label));
    if (!row.detail.empty()) {
      cells.push_back(ftxui::text("  "));
      cells.push_back(ftxui::text(row.detail) | theme::Hint() |
                      ftxui::xflex_shrink);
    }
    if (!row.note.empty()) {
      cells.push_back(ftxui::filler());
      cells.push_back(
          ftxui::text(row.note) |
          ftxui::color(row.enabled ? theme::Warning() : theme::Danger()));
    }

    Element out = ftxui::hbox(std::move(cells));
    if (current) {
      if (Focused()) {
        out = std::move(out) | ftxui::inverted;
      }
      out = std::move(out) | ftxui::focus;
    }
    return out;
  }

  // Keeps the selection inside the list and off unselectable rows.
  void Normalise() {
    const std::vector<MenuRow>& rows = *options_.rows;
    int& selected = *options_.selected;
    selected = std::clamp(selected, 0, static_cast<int>(rows.size()) - 1);
    if (rows[static_cast<size_t>(selected)].enabled) {
      return;
    }
    const int forward = Search(selected, 1);
    selected = forward >= 0 ? forward : std::max(Search(selected, -1), 0);
  }

  // The first selectable row at or after `from` in direction `delta`.
  [[nodiscard]] int Search(int from, int delta) const {
    const std::vector<MenuRow>& rows = *options_.rows;
    for (int index = from; index >= 0 && index < static_cast<int>(rows.size());
         index += delta) {
      if (rows[static_cast<size_t>(index)].enabled) {
        return index;
      }
    }
    return -1;
  }

  // Returns false at either end so the enclosing container can move the focus
  // on to the next field instead of swallowing the key.
  bool Step(int delta) { return Jump(*options_.selected + delta, delta); }

  bool Jump(int from, int delta) {
    const int next = Search(from, delta);
    if (next < 0 || next == *options_.selected) {
      return false;
    }
    *options_.selected = next;
    return true;
  }

  bool Activate() {
    const std::vector<MenuRow>& rows = *options_.rows;
    const size_t index = static_cast<size_t>(*options_.selected);
    if (index >= rows.size() || !rows[index].enabled || !options_.on_enter) {
      return false;
    }
    options_.on_enter();
    return true;
  }

  SelectListOptions options_;
};

}  // namespace

Component SelectList(SelectListOptions options) {
  return std::make_shared<SelectListBase>(std::move(options));
}

Element StepRail(std::span<const std::string_view> labels, size_t current) {
  Elements cells;
  cells.reserve(labels.size() * 2);
  for (size_t index = 0; index < labels.size(); ++index) {
    if (index > 0) {
      cells.push_back(ftxui::text(std::string(kRailSeparator)) |
                      theme::Disabled());
    }
    Element cell = ftxui::text(std::string(labels[index]));
    if (index < current) {
      cell = std::move(cell) | ftxui::color(theme::Success());
    } else if (index == current) {
      cell = std::move(cell) | theme::Heading() | ftxui::color(theme::Accent());
    } else {
      cell = std::move(cell) | theme::Disabled();
    }
    cells.push_back(std::move(cell));
  }
  return ftxui::hbox(std::move(cells));
}

}  // namespace puu::tui
