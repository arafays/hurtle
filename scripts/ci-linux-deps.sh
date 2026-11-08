#!/usr/bin/env bash
# System libraries GPUI links against on Linux (Debian/Ubuntu CI runners).
set -euo pipefail
apt-get update -q
apt-get install -y --no-install-recommends \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
  libfontconfig-dev libfreetype-dev libvulkan1 pkg-config
