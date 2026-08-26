// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_PROGRESS_H_
#define PUU_ENGINE_PROGRESS_H_

#include <cstdint>
#include <filesystem>
#include <optional>
#include <string_view>

namespace puu::engine {

// Estimates how far a long-running external command has got.
//
// A source never reports completion: the fraction is capped below 1 so that the
// step itself decides when it is done. Otherwise a bad estimate would show a
// finished bar while work continues.
class ProgressSource {
 public:
  // Highest fraction a source may report while the step is still running.
  static constexpr double kMaxFraction = 0.98;

  virtual ~ProgressSource() = default;

  // Current progress in [0, kMaxFraction], or nullopt when not yet estimable.
  virtual std::optional<double> Fraction() = 0;

  // Offers a line of command output. Sources that parse markers use it; others
  // ignore it.
  virtual void ObserveLine(std::string_view /*line*/) {}
};

// Estimates progress from free space consumed on a target filesystem. Used for
// `bootc install`, which gives no progress output of its own.
//
// The reported fraction is monotonic: transient frees do not move the bar
// backwards.
class DiskWriteProgress : public ProgressSource {
 public:
  DiskWriteProgress(std::filesystem::path mount_point, uint64_t expected_bytes);

  std::optional<double> Fraction() override;

 private:
  [[nodiscard]] std::optional<uint64_t> AvailableBytes() const;

  std::filesystem::path mount_point_;
  uint64_t expected_bytes_ = 0;
  std::optional<uint64_t> baseline_available_;
  double peak_fraction_ = 0.0;
};

// Parses "PUU_PROGRESS <done>/<total>" markers emitted by the chrooted sync
// helpers. The marker may appear anywhere in a line.
class MarkerProgress : public ProgressSource {
 public:
  std::optional<double> Fraction() override;
  void ObserveLine(std::string_view line) override;

 private:
  std::optional<double> fraction_;
};

}  // namespace puu::engine

#endif  // PUU_ENGINE_PROGRESS_H_
