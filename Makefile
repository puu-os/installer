# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

CARGO ?= cargo
CARGO_ZIGBUILD ?= cargo-zigbuild

.PHONY: all check ci clippy package publish clean

all:
	$(CARGO) build

check:
	$(CARGO) build
	$(CARGO) clippy --all-targets -- -D warnings

ci:
	CARGO="$(CARGO)" ./scripts/ci.sh

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

package:
	CARGO_ZIGBUILD="$(CARGO_ZIGBUILD)" ./scripts/package.sh $(PLATFORMS)

publish:
	./scripts/publish.sh "$(VERSION)"

clean:
	$(CARGO) clean
	$(RM) -r artifacts
