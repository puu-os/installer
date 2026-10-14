// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#ifndef PUU_ENGINE_K3S_H_
#define PUU_ENGINE_K3S_H_

#include <filesystem>

#include "base/secret_string.h"
#include "base/status.h"
#include "config.h"

namespace puu::engine {

[[nodiscard]] Status StageK3s(const std::filesystem::path& deployment,
                              const K3sConfig& config, SecretString token);

}

#endif
