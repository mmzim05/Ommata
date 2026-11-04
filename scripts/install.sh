#!/usr/bin/env bash
set -euo pipefail
# Install anyview. Default is system-wide under /usr/local (run with sudo),
# which is also what file-manager RAW thumbnails need: Nautilus runs
# thumbnailers in a sandbox that only sees /usr. PREFIX=~/.local installs for
# the current user without thumbnails. Builds the release binary if missing.

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly ROOT
readonly PREFIX="${PREFIX:-/usr/local}"
readonly BIN="$ROOT/target/release/anyview"

build_if_missing() {
  if [[ -x "$BIN" ]]; then
    return
  fi
  if (( EUID == 0 )); then
    echo "Error: build first as your user (cargo build --release)," \
      "then install with sudo" >&2
    exit 1
  fi
  "$ROOT/scripts/build-ffmpeg.sh"
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
}

install_thumbnailer() {
  local thumbnailers="$PREFIX/share/thumbnailers"
  if [[ "$PREFIX" != /usr && "$PREFIX" != /usr/* ]]; then
    echo "note: RAW thumbnails in file managers need an install under" \
      "/usr (sudo ./scripts/install.sh)"
    return
  fi
  mkdir -p "$thumbnailers"
  sed "s|@BINDIR@|$PREFIX/bin|g" "$ROOT/assets/anyview.thumbnailer" \
    > "$thumbnailers/anyview.thumbnailer"
  chmod 644 "$thumbnailers/anyview.thumbnailer"
}

# Desktop caches are optional: a missing tool or a failed refresh only delays
# the menu entry and icon until the next login.
refresh_caches() {
  if command -v update-desktop-database > /dev/null; then
    update-desktop-database -q "$PREFIX/share/applications" || true
  fi
  if command -v gtk-update-icon-cache > /dev/null; then
    gtk-update-icon-cache -qtf "$PREFIX/share/icons/hicolor" 2> /dev/null \
      || true
  fi
}

main() {
  build_if_missing
  install -Dm755 "$BIN" "$PREFIX/bin/anyview"
  install -Dm644 "$ROOT/assets/anyview.desktop" \
    "$PREFIX/share/applications/anyview.desktop"
  install -Dm644 "$ROOT/assets/anyview.svg" \
    "$PREFIX/share/icons/hicolor/scalable/apps/anyview.svg"
  install_thumbnailer
  refresh_caches
  echo "Installed anyview to $PREFIX/bin/anyview"
}

main "$@"
