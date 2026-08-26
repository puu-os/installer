// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_STEP_H_
#define PUU_ENGINE_STEP_H_

#include <cstddef>
#include <functional>
#include <span>
#include <string_view>

#include "base/status.h"
#include "engine/context.h"

namespace puu::engine {

class Runner;

// One installation step. Steps run in declaration order and are the only unit
// of progress the UI knows about.
struct Step {
  std::string_view title;
  Status (*run)(Runner& runner);

  // Relative cost, used to weight the overall progress bar. Steps that copy
  // gigabytes are weighted far above bookkeeping steps.
  int weight = 1;

  // Skipped entirely when the user declined Secure Boot enrolment.
  bool requires_secure_boot = false;

  // Writing the partition table destroys the previous contents of the drive.
  // Cancellation is refused from this step onwards.
  bool point_of_no_return = false;
};

std::span<const Step> Steps();

// Drives the pipeline and mediates between steps and the UI.
//
// The runner owns the Context for the duration of the run. Steps mutate it in
// place; in particular the format steps fill in Context::parts.
class Runner {
 public:
  // Reports progress within the current step as a fraction in [0, 1].
  using ProgressCallback =
      std::function<void(size_t step_index, double fraction)>;
  using SkipCallback = std::function<void(size_t step_index)>;
  // Returns true when the user asked to cancel. Consulted only before the
  // point of no return.
  using CancelCallback = std::function<bool()>;
  // Called once, immediately before the point-of-no-return step does any work.
  // Returns false when cancellation or UI teardown won the rendezvous.
  using CommitCallback = std::function<bool(size_t step_index)>;

  explicit Runner(Context context) : context_(std::move(context)) {}

  Context& context() { return context_; }
  const Context& context() const { return context_; }

  void set_progress_callback(ProgressCallback callback) {
    progress_callback_ = std::move(callback);
  }
  void set_skip_callback(SkipCallback callback) {
    skip_callback_ = std::move(callback);
  }
  void set_cancel_callback(CancelCallback callback) {
    cancel_callback_ = std::move(callback);
  }
  void set_commit_callback(CommitCallback callback) {
    commit_callback_ = std::move(callback);
  }

  // Runs every step in order. On failure or cancellation the mount stack is
  // unwound, but a written partition table is not restored.
  Status Execute(std::span<const Step> steps);

  void ReportProgress(double fraction) const;

  bool committed() const { return committed_; }

 private:
  Status RunStep(const Step& step, size_t index);

  Context context_;
  ProgressCallback progress_callback_;
  SkipCallback skip_callback_;
  CancelCallback cancel_callback_;
  CommitCallback commit_callback_;

  size_t current_step_ = 0;
  bool committed_ = false;
};

}  // namespace puu::engine

#endif  // PUU_ENGINE_STEP_H_
