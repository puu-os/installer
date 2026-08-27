# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

MESON ?= meson
NINJA ?= ninja
BUILDDIR ?= build

.PHONY: default help all setup check ci test format tidy package publish clean

default: help

help: ## Show this help
	@awk -F ':[^#]*## ?' '/^[a-z_-]+:[^#]*##/{printf "  make %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

setup: ## Configure the build directory
	@test -d $(BUILDDIR) && $(MESON) setup --reconfigure $(BUILDDIR) \
		|| $(MESON) setup $(BUILDDIR)

all: setup ## Build the installer
	$(NINJA) -C $(BUILDDIR)

test: all ## Run the unit tests
	$(MESON) test -C $(BUILDDIR) --print-errorlogs

check: test ## Build and test

ci: ## Run the local CI script
	./scripts/ci.sh

format: ## Reformat the sources in place
	find src -name '*.cc' -o -name '*.h' | xargs clang-format -i

tidy: setup ## Run clang-tidy over the sources
	$(NINJA) -C $(BUILDDIR) clang-tidy

package: ## Build the source release tarball and checksum
	./scripts/package.sh

publish: ## Publish a GitLab release
	./scripts/publish.sh "$(VERSION)"

clean: ## Remove build artifacts
	$(RM) -r $(BUILDDIR)
