# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026

FROM debian:trixie-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get -o APT::Retries=3 update \
    && apt-get -o APT::Retries=3 install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        clang-format \
        cmake \
        git \
        libabsl-dev \
        libblkid-dev \
        libfmt-dev \
        libftxui-dev \
        libmount-dev \
        libspdlog-dev \
        libssl-dev \
        libudev-dev \
        libyaml-cpp-dev \
        ninja-build \
        nlohmann-json3-dev \
        pkg-config \
        shellcheck \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
