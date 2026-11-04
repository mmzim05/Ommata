#!/usr/bin/env bash
set -euo pipefail
# Build a minimal static FFmpeg plus dav1d (AV1 decoding) into target/ffmpeg.
# omalens links both statically, so the final binary needs no FFmpeg at
# runtime. It does link the system's libva (GPU decoding) and x264/x265
# (re-encoding the frames before a frame-exact trim cut) dynamically:
# pacman -S libva x264 x265. x264 and x265 make this FFmpeg build GPL.

readonly VERSION="9.0"
readonly DAV1D_VERSION="1.5.4"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly ROOT
readonly SRC="$ROOT/target/ffmpeg-src"
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
# vendor's driver at runtime, and omalens falls back to the CPU without one.
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

# dav1d: AV1 on the CPU, since FFmpeg's own AV1 decoder needs the GPU.
build_dav1d() {
  local dav1d_src="$ROOT/target/dav1d-src"
  if [[ -f "$PREFIX/lib/libdav1d.a" ]]; then
    return
  fi
  if [[ ! -d "$dav1d_src" ]]; then
    git clone --depth=1 -b "$DAV1D_VERSION" \
      https://code.videolan.org/videolan/dav1d.git "$dav1d_src"
  fi
  meson setup "$dav1d_src/build" "$dav1d_src" --buildtype=release \
    --default-library=static --prefix="$PREFIX" --libdir=lib \
    -Denable_tools=false -Denable_tests=false -Denable_examples=false
  ninja -C "$dav1d_src/build" install
}

build_ffmpeg() {
  local asm=()
  if [[ ! -d "$SRC" ]]; then
    git clone --depth=1 -b "release/$VERSION" \
      https://github.com/FFmpeg/FFmpeg "$SRC"
  fi
  if ! command -v nasm > /dev/null; then
    echo "warning: nasm not found, building without SIMD" \
      "(much slower decoding)" >&2
    asm=(--disable-x86asm)
  fi
  cd "$SRC"
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
