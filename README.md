<p align="center">
  <img src="assets/anyview.svg" width="96" height="96" alt="anyview" />
</p>

<h1 align="center">anyview</h1>

<p align="center">
  <strong>One fast Linux viewer for photos, camera RAW and video.</strong><br />
  Browse a folder, scrub 4K video frame by frame, crop and rotate photos, and trim clips to the
  exact frame without losing quality, in one small native app.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/mmzim05/anyview" alt="Licence" /></a>
  <img src="https://img.shields.io/badge/platform-Linux-informational" alt="Linux" />
  <img src="https://img.shields.io/badge/built%20with-Rust-orange?logo=rust" alt="Rust" />
  <a href="https://github.com/mmzim05/anyview/stargazers"><img src="https://img.shields.io/github/stars/mmzim05/anyview?style=flat" alt="Stars" /></a>
</p>

![anyview playing a motorbike race video, paused, with the file bar on top and the seek bar at the bottom](docs/screenshots/video.webp)

*A 1080p60 race clip, paused. The bars fade out after three seconds without input and come
back when you move the mouse or press a key.*

<table>
  <tr>
    <td><img src="docs/screenshots/hover.webp" alt="Hovering the seek bar shows a thumbnail of that point" /></td>
    <td><img src="docs/screenshots/trim.webp" alt="Trim mode with in and out handles on the seek bar" /></td>
  </tr>
  <tr>
    <td align="center"><em>Hover the seek bar for a preview</em></td>
    <td align="center"><em>Trim to the exact frame</em></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/photo.webp" alt="A black and white race car photo" /></td>
    <td><img src="docs/screenshots/crop.webp" alt="Cropping a photo to 16:9" /></td>
  </tr>
  <tr>
    <td align="center"><em>Photos of any size, up to 100 MP</em></td>
    <td align="center"><em>Crop with aspect presets</em></td>
  </tr>
</table>

## What it does

- **Photos and camera RAW:** opens JPEG, PNG, WebP and more, and RAW files from 18 of the 20 cameras
  tested. RAW files show the camera's embedded preview at once; files without one develop
  at half size for the screen.
- **Video on the GPU:** decodes through VA-API and keeps frames on the GPU, so 4K60 plays at a
  fraction of a CPU core. Without a VA-API driver it decodes on the CPU.
- **Frame-accurate scrubbing:** drag the playhead and the picture follows; when you stop, it
  holds until the exact frame under the pointer is decoded.
- **Trim to the exact frame:** for H.264 and HEVC only the few frames between your in point and
  the next keyframe are re-encoded, at the bitrate the source used; the rest is copied as is.
  Quality is kept and a clip trimmed to 80% is about 80% of the size. Other codecs cut at the
  keyframe before the in point.
- **Crop, rotate and flip:** non-destructive until you save; saves keep EXIF and the colour
  profile.
- **Folder browsing:** next and previous file, move to trash, open from a file manager, and RAW
  thumbnails in file managers.
- **Looks native on Omarchy:** colours follow the active Omarchy theme and update live.

## Supported formats

✅ tested: opened and decoded by anyview's tests on a real or generated sample.
⬜ untested: should work, not checked yet. ❌ not supported.

**Photos**

| Format | Extensions | Status |
|---|---|---|
| JPEG | `.jpg` `.jpeg` `.jpe` | ✅ |
| PNG | `.png` | ✅ |
| WebP | `.webp` | ✅ |
| GIF (first frame) | `.gif` | ✅ |
| TIFF | `.tif` `.tiff` | ✅ |
| BMP | `.bmp` | ✅ |

**Camera RAW**, tested on samples from [raw.pixls.us](https://raw.pixls.us/)

| Extension | Tested camera | Status |
|---|---|---|
| `.cr2` | Canon EOS 5D Mark III | ✅ |
| `.cr3` | Canon EOS R5 Mark II | ✅ |
| `.nef` | Nikon D850 | ✅ |
| `.nrw` | Nikon Coolpix P7800 | ✅ |
| `.arw` | Sony A7R IV | ✅ |
| `.srf` | Sony DSC-F828 | ✅ |
| `.sr2` | Sony DSC-R1 | ✅ |
| `.raf` | Fujifilm X-T4 | ✅ |
| `.orf` | Olympus E-M1 Mark III | ✅ |
| `.rw2` | Panasonic DC-GH5 | ✅ |
| `.dng` | Leica M10 | ✅ |
| `.pef` | Pentax K-1 | ✅ |
| `.srw` | Samsung NX1 | ✅ |
| `.erf` | Epson R-D1 | ✅ |
| `.iiq` | Phase One IQ3 100MP | ✅ |
| `.kdc` | Kodak DC50 | ✅ |
| `.dcr` | Kodak DCS760C | ✅ |
| `.mrw` | Minolta Alpha-7 Digital | ✅ |
| `.3fr` | Hasselblad CFV-50c | ❌ camera not supported yet |
| `.mos` | Leaf AFi-II 12 | ❌ camera not supported yet |

RAW support comes from [rawler](https://github.com/dnglab/dnglab); other models of the same
brands are likely to work.

**Video**

| Container | Extensions | Codecs tested | Status |
|---|---|---|---|
| MP4 | `.mp4` `.m4v` | H.264, HEVC | ✅ |
| QuickTime | `.mov` | H.264, ProRes | ✅ |
| Matroska | `.mkv` | H.264, HEVC | ✅ |
| WebM | `.webm` | VP9, AV1 | ✅ |
| IVF | `.ivf` | VP8 | ✅ |
| AVI | `.avi` | MPEG-4 | ✅ |
| MPEG-TS | `.ts` `.mts` `.m2ts` | H.264 | ✅ |
| FLV | `.flv` | H.264 | ✅ |
| Ogg | `.ogv` | Theora | ✅ |
| ASF | `.wmv` | MS-MPEG4 v3 (WMV 9/VC-1 ⬜) | ✅ |
| MPEG-PS | `.mpg` `.mpeg` | MPEG-2, MPEG-1 | ✅ |
| 3GP | `.3gp` | H.263 | ✅ |

GPU decoding (VA-API) is tested for H.264, HEVC, VP8, VP9 and MPEG-2; MPEG-4 Part 2 and VC-1
are enabled but untested. AV1 decodes on the CPU with dav1d.

## Keys

| Key | Action |
|---|---|
| `←` `→` / `PgUp` `PgDn` | Previous / next file |
| `Space` | Play / pause |
| `Shift+←` `Shift+→`, `J` `L` | Seek ±5 s |
| `.` / `,` | Step one frame forward / back |
| `M`, `↑` `↓` | Mute, volume |
| `C` | Crop (`Enter` apply, `Esc` cancel), `1`–`5` aspect |
| `R` / `Shift+R`, `H` | Rotate right / left, flip |
| `T`, `I` / `O` | Trim mode, set in / out |
| `0`, `1`, scroll, `+` `-` | Fit, 100 %, zoom |
| `F` / `F11` / double-click | Fullscreen |
| `Del` | Move to trash |
| `Ctrl+O` | Open |

Settings (seek-bar waveform style, height and smoothing) are in the gear menu on the video bar
and saved to `~/.config/anyview/settings.conf`.

## Saving edits

Edits (crop, rotate, flip, trim) live in memory until you save:

- **Save** (`Ctrl+S`) replaces the original; the original goes to the trash as
  `name-original.ext`, so it can be restored.
- **Save copy** (`Ctrl+Shift+S`) writes `name-edit.ext` next to it (`name-edit-2.ext`, … if
  that exists). RAW files save as a JPEG copy and are never overwritten.
- **Revert** (`Ctrl+Z`) drops the edits.

## Build from source

anyview links a minimal FFmpeg statically. It uses the system's libva for GPU decoding and
x264/x265 for frame-exact trims.

```sh
sudo pacman -S --needed meson ninja nasm libva x264 x265
./scripts/build-ffmpeg.sh       # minimal static FFmpeg into target/ffmpeg
cargo build --release           # target/release/anyview
sudo ./scripts/install.sh       # install to /usr/local (PREFIX=~/.local for one user)
```

For GPU decoding, install your GPU's VA-API driver (Intel: `intel-media-driver`, AMD:
`libva-mesa-driver`) and run a Wayland session. Development against the system FFmpeg, with
no build script:

```sh
FFMPEG_DIR=/usr cargo run --no-default-features -- <file-or-folder>
```

## License

GPL-3.0-or-later. You may use, change and share anyview; versions you distribute, modified or
not, must stay under the same license with their source available. See [LICENSE](LICENSE).

Screenshot media from [Pexels](https://www.pexels.com/): photos by Philipp Fahlbusch and Telmo
Antunes, and a motorbike race video.
