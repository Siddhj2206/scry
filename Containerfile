# syntax=docker/dockerfile:1
#
# Build & headless Wayland test environment for scry.
#
#   podman build --target build -t scry-build .
#   podman build --target e2e   -t scry-e2e   .
#
# `build` is the lightweight compile/unit-test stage: Rust toolchain plus the
# C dev headers needed by Wayland-adjacent crates, plus dbus/at-spi2 for
# headless a11y tests.
# `e2e` layers real Wayland clients (foot, zenity, wl-clipboard) on top for
# full end-to-end tests against a headless compositor.

# ---------------------------------------------------------------------------
# Stage: build — minimal Rust + C dev headers + headless runtime test deps
# ---------------------------------------------------------------------------
FROM docker.io/library/rust:1-slim-trixie AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        # Build deps
        pkg-config \
        libxkbcommon-dev \
        libpixman-1-dev \
        libwayland-dev \
        # Headless runtime & E2E test deps
        dbus \
        at-spi2-core \
        xkb-data \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace

ENV XDG_RUNTIME_DIR=/tmp/xdg-runtime \
    GSK_RENDERER=cairo

RUN mkdir -p /tmp/xdg-runtime && chmod 0700 /tmp/xdg-runtime

# ---------------------------------------------------------------------------
# Stage: e2e — adds real Wayland clients (pulls in GTK etc., kept out of build)
# ---------------------------------------------------------------------------
FROM build AS e2e

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        foot \
        zenity \
        wl-clipboard \
        # foot fails to start without at least one font on the system.
        fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*
