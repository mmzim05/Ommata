#!/usr/bin/env bash
set -euo pipefail
# Package omalens as an AppImage in target/appimage, with linuxdeploy.
# An AppImage only runs on systems whose glibc is at least as new as the
# build machine's. x264 and x265 are bundled; libva and libdrm come from the
# host so they match its GPU driver, and graphics and audio libraries always
# come from the host.

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
  # appimagetool makes .DirIcon, the icon AppImage installers and file
  # managers show, only from a PNG at the top of the AppDir.
  local icon_png="$OUT/omalens.png"
  rsvg-convert -w 256 -h 256 -o "$icon_png" "$ROOT/assets/omalens.svg"
  mkdir -p "$APPDIR"
  cp "$icon_png" "$APPDIR/omalens.png"
  # Extract-and-run: works without FUSE, as in containers and CI.
  # appimagetool writes the .zsync file to the working directory.
  cd "$OUT"
  # Update information lets Gear Lever and AppImageUpdate fetch new releases
  # (only the changed blocks, through the .zsync file published alongside).
  APPIMAGE_EXTRACT_AND_RUN=1 \
    LDAI_OUTPUT="$OUT/omalens-$(version)-x86_64.AppImage" \
    LDAI_UPDATE_INFORMATION="gh-releases-zsync|mmzim05|omalens|latest|omalens-*-x86_64.AppImage.zsync" \
    "$linuxdeploy" \
    --appdir "$APPDIR" \
    --executable "$ROOT/target/release/omalens" \
    --desktop-file "$ROOT/assets/omalens.desktop" \
    --icon-file "$ROOT/assets/omalens.svg" \
    --icon-file "$icon_png" \
    --exclude-library 'libva*' \
    --exclude-library 'libdrm*' \
    --output appimage
  echo "Built $OUT/omalens-$(version)-x86_64.AppImage"
}

main "$@"
