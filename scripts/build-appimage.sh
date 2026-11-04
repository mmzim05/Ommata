#!/usr/bin/env bash
set -euo pipefail
# Package anyview as an AppImage in target/appimage, with linuxdeploy.
# An AppImage only runs on systems whose glibc is at least as new as the
# build machine's, so release builds run on an old distro (the release
# workflow uses Ubuntu 22.04). x264 and x265 are bundled; libva and libdrm
# come from the host so they match its GPU driver, and graphics and audio
# libraries always come from the host.

readonly LINUXDEPLOY_URL="https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly ROOT
readonly OUT="$ROOT/target/appimage"
readonly APPDIR="$OUT/AppDir"

# The installed linuxdeploy (AUR: linuxdeploy-appimage), else a download.
find_linuxdeploy() {
  local download="$OUT/linuxdeploy-x86_64.AppImage"
  if command -v linuxdeploy > /dev/null; then
    command -v linuxdeploy
    return
  fi
  if [[ ! -x "$download" ]]; then
    curl -fL --retry 3 -o "$download" "$LINUXDEPLOY_URL"
    chmod +x "$download"
  fi
  echo "$download"
}

version() {
  sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1
}

main() {
  "$ROOT/scripts/build-ffmpeg.sh"
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
  mkdir -p "$OUT"
  rm -rf "$APPDIR"
  local linuxdeploy
  linuxdeploy="$(find_linuxdeploy)"
  # Extract-and-run: works without FUSE, as in containers and CI.
  APPIMAGE_EXTRACT_AND_RUN=1 \
    LDAI_OUTPUT="$OUT/anyview-$(version)-x86_64.AppImage" \
    "$linuxdeploy" \
    --appdir "$APPDIR" \
    --executable "$ROOT/target/release/anyview" \
    --desktop-file "$ROOT/assets/anyview.desktop" \
    --icon-file "$ROOT/assets/anyview.svg" \
    --exclude-library 'libva*' \
    --exclude-library 'libdrm*' \
    --output appimage
  echo "Built $OUT/anyview-$(version)-x86_64.AppImage"
}

main "$@"
