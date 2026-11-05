<div align="center">

<img src="assets/omalens.svg" width="112" height="112" alt="omalens logo" />

# omalens

**One fast viewer for your photos, camera RAW files and videos on Linux.**

Open it, look, trim or crop, move on. No library to import into, no separate apps for
pictures and video.

![Linux](https://img.shields.io/badge/platform-Linux-informational?logo=linux&logoColor=white)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)

[Install](#install) · [Features](#features) · [Usage](#usage) ·
[Supported files](#supported-files) · [Report a problem](#reporting-problems) ·
[Build from source](#build-from-source) · [Changelog](CHANGELOG.md)

<img src="docs/screenshots/video.webp" alt="Stepping through photos and videos in a folder with omalens" width="900" />

</div>

## Why omalens

Most Linux setups use one app for photos, another for video, and a third (or a terminal
full of `ffmpeg` flags) to cut a clip. omalens does all three in one window that stays out of
your way.

## Features

### Photos and camera RAW

Open JPEG, PNG, WebP and RAW files from most cameras. Zoom to actual pixels, fit to the
window, or go fullscreen. EXIF rotation is applied for you.

<img src="docs/screenshots/photo.webp" alt="Zooming into a photo and stepping to a RAW file" width="800" />

### Smooth video

Play and scrub even 4K video. Hover the seek bar to preview any moment, and step frame by
frame with `,` and `.`.

<img src="docs/screenshots/hover.webp" alt="Scrubbing a video with seek-bar thumbnails" width="800" />

### Trim clips to the exact frame

Press `T`, drag the handles or press `I` and `O`, then save. The video is copied, not
re-encoded, so it saves in a fraction of a second with no loss in quality. MP4 and MOV
clips start on exactly the frame you chose, in any codec.

<img src="docs/screenshots/trim.webp" alt="Trimming a video and saving a copy" width="800" />

### Crop, rotate and flip

Press `C` to crop freely or to a fixed shape such as 16:9, `R` to rotate and `H` to flip.
Save over the original (it goes to the trash, not away) or keep a copy.

<img src="docs/screenshots/crop.webp" alt="Cropping a photo to 16:9 and saving a copy" width="800" />

### Tags, ratings and colour labels

Sort a shoot as you step through it: one key adds a tag, rates a clip or rejects it, and
**Filter** then shows only the files you want. Tags live in XMP sidecar files next to your
media (`clip.MOV.xmp`), never inside it, and use the same fields as digiKam, darktable,
Lightroom and exiftool, so they keep working in those apps too. An existing sidecar keeps
everything else in it, like a darktable edit history.

### And also

- **Browse a folder:** step through every photo and video with the arrow keys.
- **Thumbnails in your file manager,** including RAW files, when installed system-wide.
- **Controls that get out of the way:** they fade when you stop moving the mouse.

## Install

### AppImage

Download `omalens-<version>-x86_64.AppImage` from the
[latest release](https://github.com/mmzim05/omalens/releases/latest), make it executable,
and open it:

```sh
chmod +x omalens-*-x86_64.AppImage
./omalens-*-x86_64.AppImage
```

To add it to your app menu, open it with [Gear Lever](https://flathub.org/apps/it.mijorus.gearlever).

**Updates:** omalens checks for a new release once a day. When there is one, an
**Update** button appears in the top bar: it downloads the new version, and
**Restart to update** opens it on the same file. Gear Lever can update it too.

### From source

See [Build from source](#build-from-source). Installing this way also gives you RAW
thumbnails in your file manager.

### For smooth video

Install your graphics driver's video package, otherwise video decodes on the CPU:

| GPU   | Package              |
| ----- | -------------------- |
| Intel | `intel-media-driver` |
| AMD   | `libva-mesa-driver`  |

## Usage

Open a photo or video from your file manager with omalens, or from a terminal:

```sh
omalens photo.jpg        # open a file, with its folder ready to browse
omalens ~/Pictures/trip  # open a folder
```

Press `?` for Help: the keyboard shortcuts, your version and updates, and reporting a
problem. Settings are behind the gear in the top bar, or on the start screen.

### Keyboard shortcuts

| Browse        |                    | View            |                              |
| ------------- | ------------------ | --------------- | ---------------------------- |
| `←` `→`       | Previous / next    | Scroll, `+` `-` | Zoom                         |
| `Home` `End`  | First / last       | `0` / `1`       | Fit to window / actual size  |
| `Del`         | Move to trash      | `F`             | Fullscreen                   |
| `Ctrl+O`      | Open a file        | `?`             | Help                         |
| `F2`          | Rename             |                 |                              |

| Video        |                           | Photo         |                            |
| ------------ | ------------------------- | ------------- | -------------------------- |
| `Space`      | Play / pause              | `C`           | Crop                       |
| `J` `L`      | Back / forward 5 seconds  | `1` – `5`     | Crop shape, while cropping |
| `,` `.`      | Back / forward one frame  | `Enter`       | Apply crop                 |
| `M`          | Mute                      | `R` `Shift+R` | Rotate right / left        |
| `↑` `↓`      | Volume                    | `H`           | Flip                       |
| `T`          | Trim                      | `Esc`         | Cancel crop                |
| `I` `O`      | Set trim start / end      |               |                            |
| `Esc`        | Leave trim                |               |                            |

| Save           |                                                     |
| -------------- | --------------------------------------------------- |
| `Ctrl+S`       | Save over the original (the original goes to trash) |
| `Ctrl+Shift+S` | Save a copy next to it                              |
| `Ctrl+Z`       | Undo all edits                                      |
| `Ctrl+Q`       | Quit                                                |

| Tags           |                                            |
| -------------- | ------------------------------------------ |
| `Alt+1` – `9`  | Quick tag on or off (set them in Settings) |
| `Ctrl+1` – `5` | Rating; the same rating again clears it    |
| `Ctrl+0`       | Clear the rating                           |
| `X`            | Reject                                     |
| `Ctrl+6` – `9` | Red, yellow, green or blue label           |
| `Ctrl+K`       | Tags panel                                 |
| `Ctrl+F`       | Filter                                     |

Edits to a camera RAW save as a DNG: all the original sensor data, with the crop and
rotation stored in the file for RAW editors (Lightroom, darktable, RawTherapee) to apply.
Saving over a RAW writes `name.dng` and moves the original to the trash.

To rename a file, click its name in the top bar or press `F2`, type, and press Enter.

`Del` asks before moving a file to the trash. To clear out a folder quickly, tick
**Don't ask again until omalens closes**.

## Reporting problems

Press `?`, open **Report an issue**, and click **Report an issue**. It opens a
[bug report](https://github.com/mmzim05/omalens/issues/new/choose) with your omalens
version, system and GPU already filled in; describe what happened and, if you can, which
file it happened with. Ideas are welcome as feature requests.

## Supported files

| Kind       | Formats                                                                                                                           |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------- |
| Photos     | JPEG, PNG, WebP, GIF, TIFF, BMP                                                                                                   |
| Camera RAW | Canon, Nikon, Sony, Fujifilm, Olympus, Panasonic, Pentax, Samsung, Leica and other DNG, Phase One, Kodak, Epson, Minolta          |
| Video      | MP4, MOV, MKV, WebM, AVI, MPEG-TS, MPG, FLV, OGV, WMV, 3GP                                                                        |

Hasselblad (`.3fr`) and Leaf (`.mos`) RAW files do not open yet.

## Build from source

On Arch:

```sh
sudo pacman -S --needed git rust meson ninja nasm libva x264 x265
git clone https://github.com/mmzim05/omalens
cd omalens
./scripts/build-ffmpeg.sh   # a minimal FFmpeg, linked into the binary
cargo build --release
sudo ./scripts/install.sh   # installs to /usr/local, with file-manager thumbnails
```

`cargo release patch --execute` tags a new version (see [CHANGELOG.md](CHANGELOG.md)),
and `./scripts/build-appimage.sh` builds its AppImage and `.zsync` for the GitHub release.

## Licence

[GPL-3.0-or-later](LICENSE)

Demo media from [Pexels](https://www.pexels.com/): photos by Philipp Fahlbusch and Telmo
Antunes, and a motorbike race video.
