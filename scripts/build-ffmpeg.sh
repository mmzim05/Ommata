#!/usr/bin/env bash
set -euo pipefail
# Build a minimal static FFmpeg plus dav1d (AV1 decoding) into target/ffmpeg.
# ommata links both statically, so the final binary needs no FFmpeg at
# runtime. It does link the system's libva (GPU decoding) and x264/x265
# (re-encoding the frames before a frame-exact trim cut) dynamically:
# pacman -S libva x264 x265. x264 and x265 make this FFmpeg build GPL.
#
# The sources are pinned release tarballs, downloaded and checked here. A
# package build that has unpacked them already (PKGBUILD sources) passes
# their directories in FFMPEG_SOURCE and DAV1D_SOURCE and builds offline.

readonly VERSION="9.0.2"
readonly SHA256="8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e"
readonly DAV1D_VERSION="1.5.4"
readonly DAV1D_SHA256="686616b7c69eb88d44459391ab25cac13b6647a3b288835c5784e71c1514a5c5"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly ROOT
readonly PREFIX="$ROOT/target/ffmpeg"

# MPEG-PS identifies its video streams by probing them with the raw-stream
# demuxers (mpegvideo, h264, hevc); without them a stream can come out as MP3.
readonly DEMUXERS=(mov matroska avi mpegts mpegps flv ogg asf ivf mp3 wav aac
  flac mpegvideo h264 hevc)
readonly MUXERS=(mov mp4 matroska webm avi mpegts ogg flv asf ivf)
# AV1 has two decoders: FFmpeg's own (av1) decodes only on the GPU, and
# libdav1d on the CPU when the GPU can't.
readonly DECODERS=(
  h264 hevc vp8 vp9 av1 libdav1d mpeg4 mpeg1video mpeg2video mjpeg prores theora
  vc1 wmv3 h263 msmpeg4v3 dvvideo
  aac aac_latm mp3 mp3float mp2 opus vorbis flac ac3 eac3 alac dca truehd wmav2
  pcm_s16le pcm_s16be pcm_s24le pcm_s32le pcm_f32le pcm_u8
)
readonly PARSERS=(h264 hevc vp8 vp9 av1 mpeg4video mpegvideo mjpeg vc1 aac
  aac_latm mpegaudio opus vorbis flac ac3 dca)
# GPU decoding through VA-API. libva is linked dynamically: it loads the GPU
# vendor's driver at runtime, and ommata falls back to the CPU without one.
readonly HWACCELS=(h264_vaapi hevc_vaapi vp8_vaapi vp9_vaapi av1_vaapi
  mpeg2_vaapi mpeg4_vaapi vc1_vaapi wmv3_vaapi)

join() {
  local IFS=,
  echo "$*"
}

check_tools() {
  local tool
  for tool in meson ninja; do
    if ! command -v "$tool" > /dev/null; then
      echo "Error: $tool is required (pacman -S meson ninja)" >&2
      exit 1
    fi
  done
}

# Print the source directory of a pinned tarball: `given` if set, else
# `url` downloaded into target/, checked against `sha256` and unpacked.
fetch() {
  local given="$1" url="$2" sha256="$3"
  local archive="$ROOT/target/${url##*/}"
  local dir="${archive%.tar.xz}"
  if [[ -n "$given" ]]; then
    echo "$given"
    return
  fi
  if [[ ! -d "$dir" ]]; then
    mkdir -p "$ROOT/target"
    if [[ ! -f "$archive" ]]; then
      curl -fL --retry 3 -o "$archive.part" "$url" >&2
      mv "$archive.part" "$archive"
    fi
    if ! echo "$sha256  $archive" | sha256sum -c --quiet >&2; then
      echo "Error: checksum mismatch for $archive" >&2
      exit 1
    fi
    tar -xJf "$archive" -C "$ROOT/target"
  fi
  echo "$dir"
}

# dav1d: AV1 on the CPU, since FFmpeg's own AV1 decoder needs the GPU.
build_dav1d() {
  local dav1d_src
  if [[ -f "$PREFIX/lib/libdav1d.a" ]]; then
    return
  fi
  dav1d_src="$(fetch "${DAV1D_SOURCE:-}" \
    "https://downloads.videolan.org/pub/videolan/dav1d/$DAV1D_VERSION/dav1d-$DAV1D_VERSION.tar.xz" \
    "$DAV1D_SHA256")"
  meson setup "$dav1d_src/build" "$dav1d_src" --buildtype=release \
    --default-library=static --prefix="$PREFIX" --libdir=lib \
    -Denable_tools=false -Denable_tests=false -Denable_examples=false
  ninja -C "$dav1d_src/build" install
}

build_ffmpeg() {
  local asm=() src
  src="$(fetch "${FFMPEG_SOURCE:-}" \
    "https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz" "$SHA256")"
  if ! command -v nasm > /dev/null; then
    echo "warning: nasm not found, building without SIMD" \
      "(much slower decoding)" >&2
    asm=(--disable-x86asm)
  fi
  cd "$src"
  PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" ./configure \
    --prefix="$PREFIX" \
    --enable-static --disable-shared --enable-pic --enable-pthreads \
    --disable-everything --disable-autodetect --disable-programs \
    --disable-doc --disable-network --disable-avdevice --disable-avfilter \
    --pkg-config-flags=--static --enable-libdav1d \
    --enable-gpl --enable-libx264 --enable-libx265 \
    --enable-encoder=libx264,libx265 \
    --enable-protocol=file \
    --enable-demuxer="$(join "${DEMUXERS[@]}")" \
    --enable-muxer="$(join "${MUXERS[@]}")" \
    --enable-decoder="$(join "${DECODERS[@]}")" \
    --enable-parser="$(join "${PARSERS[@]}")" \
    --enable-vaapi --enable-hwaccel="$(join "${HWACCELS[@]}")" \
    --enable-bsfs \
    --enable-swscale --enable-swresample \
    "${asm[@]}"
  make -j"$(nproc)"
  make install
}

main() {
  if [[ -f "$PREFIX/lib/libavcodec.a" ]]; then
    echo "FFmpeg already built in $PREFIX (delete it to rebuild)"
    exit 0
  fi
  check_tools
  build_dav1d
  build_ffmpeg
  # Rust bundles the static libraries into ffmpeg-sys-next's rlib, so a
  # rebuilt FFmpeg only reaches the binary once that crate is rebuilt.
  cargo clean --release -p ffmpeg-sys-next --manifest-path "$ROOT/Cargo.toml"
  cargo clean -p ffmpeg-sys-next --manifest-path "$ROOT/Cargo.toml"
}

main "$@"
