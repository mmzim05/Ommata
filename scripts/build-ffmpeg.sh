#!/usr/bin/env bash
# Build a minimal static FFmpeg (decode + remux only) plus dav1d (AV1) into
# target/ffmpeg. anyview links both statically, so the final binary needs no
# FFmpeg at runtime.
set -euo pipefail

VERSION=9.0
DAV1D_VERSION=1.5.4
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SRC=$ROOT/target/ffmpeg-src
PREFIX=$ROOT/target/ffmpeg

if [[ -f $PREFIX/lib/libavcodec.a ]]; then
  echo "FFmpeg already built in $PREFIX (delete it to rebuild)"
  exit 0
fi

for tool in meson ninja; do
  command -v $tool >/dev/null || { echo "error: $tool is required (pacman -S meson ninja)" >&2; exit 1; }
done

# dav1d: FFmpeg's own AV1 decoder only works with hardware acceleration.
if [[ ! -f $PREFIX/lib/libdav1d.a ]]; then
  DAV1D_SRC=$ROOT/target/dav1d-src
  [[ -d $DAV1D_SRC ]] || git clone --depth=1 -b "$DAV1D_VERSION" https://code.videolan.org/videolan/dav1d.git "$DAV1D_SRC"
  meson setup "$DAV1D_SRC/build" "$DAV1D_SRC" --buildtype=release --default-library=static \
    --prefix="$PREFIX" --libdir=lib -Denable_tools=false -Denable_tests=false -Denable_examples=false
  ninja -C "$DAV1D_SRC/build" install
fi
export PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig

if [[ ! -d $SRC ]]; then
  git clone --depth=1 -b "release/$VERSION" https://github.com/FFmpeg/FFmpeg "$SRC"
fi

asm=()
if ! command -v nasm >/dev/null; then
  echo "warning: nasm not found, building without SIMD (much slower decoding)" >&2
  asm=(--disable-x86asm)
fi

join() { local IFS=,; echo "$*"; }

demuxers=(mov matroska avi mpegts mpegps flv ogg asf ivf mp3 wav aac flac)
muxers=(mov mp4 matroska webm avi mpegts ogg flv asf ivf)
decoders=(
  h264 hevc vp8 vp9 libdav1d mpeg4 mpeg1video mpeg2video mjpeg prores theora vc1 wmv3 h263 msmpeg4v3 dvvideo
  aac aac_latm mp3 mp3float mp2 opus vorbis flac ac3 eac3 alac dca truehd wmav2
  pcm_s16le pcm_s16be pcm_s24le pcm_s32le pcm_f32le pcm_u8
)
parsers=(h264 hevc vp8 vp9 av1 mpeg4video mpegvideo mjpeg vc1 aac aac_latm mpegaudio opus vorbis flac ac3 dca)

cd "$SRC"
./configure \
  --prefix="$PREFIX" \
  --enable-static --disable-shared --enable-pic --enable-pthreads \
  --disable-everything --disable-autodetect --disable-programs --disable-doc \
  --disable-network --disable-avdevice --disable-avfilter \
  --pkg-config-flags=--static --enable-libdav1d \
  --enable-protocol=file \
  --enable-demuxer="$(join "${demuxers[@]}")" \
  --enable-muxer="$(join "${muxers[@]}")" \
  --enable-decoder="$(join "${decoders[@]}")" \
  --enable-parser="$(join "${parsers[@]}")" \
  --enable-bsfs \
  --enable-swscale --enable-swresample \
  "${asm[@]}"
make -j"$(nproc)"
make install
