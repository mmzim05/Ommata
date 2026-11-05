#!/usr/bin/env bash
set -euo pipefail
# Install ommata. Default is system-wide under /usr/local (run with sudo),
# which is also what file-manager RAW thumbnails need: Nautilus runs
# thumbnailers in a sandbox that only sees /usr. PREFIX=~/.local installs for
# the current user without thumbnails. Builds the release binary if missing.
# A package build sets PREFIX=/usr and DESTDIR to its staging directory; the
# binary must be built already, and pacman's hooks refresh the caches.

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly ROOT
readonly PREFIX="${PREFIX:-/usr/local}"
readonly DESTDIR="${DESTDIR:-}"
readonly DEST="$DESTDIR$PREFIX"
readonly BIN="$ROOT/target/release/ommata"

build_if_missing() {
  if [[ -x "$BIN" ]]; then
    return
  fi
  if [[ -n "$DESTDIR" ]]; then
    echo "Error: build first (cargo build --release)" >&2
    exit 1
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
  local thumbnailers="$DEST/share/thumbnailers"
  if [[ "$PREFIX" != /usr && "$PREFIX" != /usr/* ]]; then
    echo "note: RAW thumbnails in file managers need an install under" \
      "/usr (sudo ./scripts/install.sh)"
    return
  fi
  mkdir -p "$thumbnailers"
  sed "s|@BINDIR@|$PREFIX/bin|g" "$ROOT/assets/ommata.thumbnailer" \
    > "$thumbnailers/ommata.thumbnailer"
  chmod 644 "$thumbnailers/ommata.thumbnailer"
}

# Desktop caches are optional: a missing tool or a failed refresh only delays
# the menu entry and icon until the next login.
refresh_caches() {
  if [[ -n "$DESTDIR" ]]; then
    return
  fi
  if command -v update-desktop-database > /dev/null; then
    update-desktop-database -q "$DEST/share/applications" || true
  fi
  if command -v gtk-update-icon-cache > /dev/null; then
    gtk-update-icon-cache -qtf "$DEST/share/icons/hicolor" 2> /dev/null \
      || true
  fi
}

main() {
  build_if_missing
  install -Dm755 "$BIN" "$DEST/bin/ommata"
  install -Dm644 "$ROOT/assets/ommata.desktop" \
    "$DEST/share/applications/ommata.desktop"
  install -Dm644 "$ROOT/assets/ommata.svg" \
    "$DEST/share/icons/hicolor/scalable/apps/ommata.svg"
  install_thumbnailer
  refresh_caches
  echo "Installed Ommata to $DEST/bin/ommata"
}

main "$@"
