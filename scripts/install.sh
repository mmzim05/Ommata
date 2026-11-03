#!/usr/bin/env bash
# Install anyview. Default is system-wide under /usr/local (run with sudo),
# which is also what file-manager RAW thumbnails need: Nautilus runs
# thumbnailers in a sandbox that only sees /usr. PREFIX=~/.local installs for
# the current user without thumbnails. Builds the release binary if missing.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PREFIX=${PREFIX:-/usr/local}
BIN=$ROOT/target/release/anyview

if [[ ! -x $BIN && $EUID -eq 0 ]]; then
  echo "error: build first as your user (cargo build --release), then install with sudo" >&2
  exit 1
fi
if [[ ! -x $BIN ]]; then
  "$ROOT/scripts/build-ffmpeg.sh"
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
fi

install -Dm755 "$BIN" "$PREFIX/bin/anyview"
install -Dm644 "$ROOT/assets/anyview.desktop" "$PREFIX/share/applications/anyview.desktop"
install -Dm644 "$ROOT/assets/anyview.svg" "$PREFIX/share/icons/hicolor/scalable/apps/anyview.svg"
if [[ $PREFIX == /usr || $PREFIX == /usr/* ]]; then
  mkdir -p "$PREFIX/share/thumbnailers"
  sed "s|@BINDIR@|$PREFIX/bin|g" "$ROOT/assets/anyview.thumbnailer" >"$PREFIX/share/thumbnailers/anyview.thumbnailer"
  chmod 644 "$PREFIX/share/thumbnailers/anyview.thumbnailer"
else
  echo "note: RAW thumbnails in file managers need an install under /usr (sudo ./scripts/install.sh)"
fi

command -v update-desktop-database >/dev/null && update-desktop-database -q "$PREFIX/share/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf "$PREFIX/share/icons/hicolor" 2>/dev/null || true

echo "Installed anyview to $PREFIX/bin/anyview"
