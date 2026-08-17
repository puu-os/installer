# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

CARGO ?= cargo
CARGO_ZIGBUILD ?= cargo-zigbuild
LINUX_CARGO = CARGO="$(CARGO)" CARGO_ZIGBUILD="$(CARGO_ZIGBUILD)" ./scripts/linux-cargo.sh

.PHONY: default help all check ci clippy package publish clean

default: help

help: ## Show this help
	@awk -F ':[^#]*## ?' '/^[a-z_-]+:[^#]*##/{printf "  make %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

all: ## Build the installer
	$(LINUX_CARGO) build

check: ## Build and lint with clippy
	$(LINUX_CARGO) build
	$(LINUX_CARGO) clippy --all-targets -- -D warnings

ci: ## Run the local CI script
	CARGO="$(CARGO)" CARGO_ZIGBUILD="$(CARGO_ZIGBUILD)" ./scripts/ci.sh

clippy: ## Lint with clippy
	$(LINUX_CARGO) clippy --all-targets -- -D warnings

package: ## Package release tarballs
	CARGO_ZIGBUILD="$(CARGO_ZIGBUILD)" ./scripts/package.sh $(PLATFORMS)

publish: ## Publish a GitLab release
	./scripts/publish.sh "$(VERSION)"

clean: ## Remove build artifacts
	$(CARGO) clean
	$(RM) -r artifacts
