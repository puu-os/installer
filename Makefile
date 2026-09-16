# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

CMAKE ?= cmake
CTEST ?= ctest
BUILDDIR ?= build

.PHONY: default help all setup check ci test format publish clean

default: help

help: ## Show this help
	@awk -F ':[^#]*## ?' '/^[a-z_-]+:[^#]*##/{printf "  make %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

setup: ## Configure the build directory
	$(CMAKE) -S . -B $(BUILDDIR) -G Ninja

all: setup ## Build the installer
	$(CMAKE) --build $(BUILDDIR)

test: all ## Run the unit tests
	$(CTEST) --test-dir $(BUILDDIR) --output-on-failure

check: test ## Build and test

ci: ## Build and test Linux arm64 and amd64 in containers
	./scripts/ci.sh

format: ## Reformat the sources in place
	find src -name '*.cc' -o -name '*.h' | xargs clang-format -i

publish: ## Publish a GitLab release
	./scripts/publish.sh "$(VERSION)"

clean: ## Remove build artifacts
	$(RM) -r $(BUILDDIR)
