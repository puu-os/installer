// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include <algorithm>
#include <cstddef>
#include <memory>
#include <span>
#include <string>

#include <spdlog/spdlog.h>

#include "base/status.h"
#include "engine/context.h"
#include "engine/pipeline.h"
#include "engine/step.h"

namespace puu::engine {

Status Runner::Execute(std::span<const Step> steps) {
  for (size_t index = 0; index < steps.size(); ++index) {
    const Step& step = steps[index];

    if (!committed_ && cancel_callback_ && cancel_callback_()) {
      InstallationLogger()->info("installation cancelled before {}",
                                 step.title);
      Rollback();
      return absl::AbortedError("installation cancelled");
    }
    if (step.requires_secure_boot &&
        context_.secure_boot == SecureBootMode::kSkip) {
      if (skip_callback_) skip_callback_(index);
      continue;
    }

    const Status status = RunStep(step, index);
    if (!status.ok()) {
      Rollback();
      return WithContext(status, step.title);
    }
  }
  return absl::OkStatus();
}

Status Runner::RunStep(const Step& step, size_t index) {
  current_step_ = index;
  ReportProgress(0.0);
  if (step.point_of_no_return) {
    InstallationLogger()->info("entering point of no return");

    if (commit_callback_ && !commit_callback_(index)) {
      return absl::CancelledError("installation cancelled before disk write");
    }
    committed_ = true;
  }
  try {
    PUU_RETURN_IF_ERROR(step.run(*this));
  } catch (const std::exception& e) {
    return absl::InternalError(e.what());
  } catch (...) {
    return absl::InternalError("unexpected exception during step");
  }
  ReportProgress(1.0);
  return absl::OkStatus();
}

void Runner::ReportProgress(double fraction) const {
  if (!progress_callback_) return;
  progress_callback_(current_step_, std::clamp(fraction, 0.0, 1.0));
}

}  // namespace puu::engine
