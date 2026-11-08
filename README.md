<div align="center">

<img src="assets/ommata.svg" width="112" height="112" alt="Ommata logo" />

# Ommata

*Ommata* (ὄμματα, said *OM-ma-ta*) is Greek for "eyes". It was called omalens up to version 0.3.1.

**One fast viewer for your photos, camera RAW files and videos on Linux.**

Open it, look, trim or crop, move on. No library to import into, no separate apps for
pictures and video.

![Linux](https://img.shields.io/badge/platform-Linux-informational?logo=linux&logoColor=white)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)

[Install](#install) · [Features](#features) · [Usage](#usage) ·
[Supported files](#supported-files) · [Report a problem](#reporting-problems) ·
[Build from source](#build-from-source) · [Changelog](CHANGELOG.md)

<img src="docs/screenshots/video.webp" alt="Stepping through photos and videos in a folder with Ommata" width="900" />

</div>

## Why Ommata

Most Linux setups use one app for photos, another for video, and a third (or a terminal
full of `ffmpeg` flags) to cut a clip. Ommata does all three in one window that stays out of
your way.

## Features

### Photos and camera RAW

Open JPEG, PNG, WebP and RAW files from most cameras. Zoom to actual pixels, fit to the
window, or go fullscreen. EXIF rotation is applied for you.

<img src="docs/screenshots/photo.webp" alt="Zooming into a photo and stepping to a RAW file" width="800" />

### Smooth video

Play and scrub even 4K video. Hover the seek bar to preview any moment, step frame by
frame with `Alt+,` and `Alt+.`, and zoom in with the scroll wheel or `Alt+=` like on a photo.

<img src="docs/screenshots/hover.webp" alt="Scrubbing a video with seek-bar thumbnails" width="800" />

### Trim clips to the exact frame

Press `Alt+T`, drag the handles or press `Alt+I` and `Alt+O`, then save. The video is copied, not
re-encoded, so it saves in a fraction of a second with no loss in quality. MP4 and MOV
clips start on exactly the frame you chose, in any codec.

<img src="docs/screenshots/trim.webp" alt="Trimming a video and saving a copy" width="800" />

### Crop, rotate and flip

Press `Alt+C` to crop freely or to a fixed shape such as 16:9, `Alt+R` to rotate and `Alt+H`
to flip. Save over the original (it goes to the trash, not away) or keep a copy.

Videos rotate too. MP4, MOV, MKV and WebM save the turn in the file without re-encoding, so
saving is instant and loses nothing. Phone videos recorded on their side show the right way
up.

<img src="docs/screenshots/crop.webp" alt="Cropping a photo to 16:9 and saving a copy" width="800" />

### Tags, ratings and colour labels

Sort a shoot as you step through it: one key adds a tag, rates a clip or rejects it, and
**Filter** then shows only the files you want. The tag button (`Ctrl+K`) opens a small bar
under the top bar with the rating, colour labels, a reject button and a field to type new
tags. While you type, it suggests the tags you have used before, in any folder: `↑` `↓`
pick one, `Tab` fills it in and `Enter` adds it. **Edit tags** in it is where you remove
tags, search them all and see every tag you have used. Tags live in XMP sidecar files next to
your media (`clip.MOV.xmp`), never inside it, and use the same fields as digiKam, darktable,
Lightroom and exiftool, so they keep working in those apps too. An existing sidecar keeps
everything else in it, like a darktable edit history.

### And also

- **Browse a folder:** step through every photo and video with the arrow keys.
- **Thumbnails in your file manager,** including RAW files, when installed system-wide.
- **Controls that get out of the way:** they fade when you stop moving the mouse.
- **Media keys:** play, pause, next and previous work from your keyboard, `playerctl` and
  your desktop's player controls.
- **One click to make it your viewer:** **Set Ommata as default** in Settings makes it
  the default app for every type it opens.

## Install

### AppImage

Download `ommata-<version>-x86_64.AppImage` from the
[latest release](https://github.com/mmzim05/Ommata/releases/latest), make it executable,
and open it:

```sh
chmod +x ommata-*-x86_64.AppImage
./ommata-*-x86_64.AppImage
```

To add it to your app menu, open it with [Gear Lever](https://flathub.org/apps/it.mijorus.gearlever).

**Updates:** Ommata checks for a new release each time it opens. When there is one, a
card in the corner says so, and an **Update** button in it and in the top bar downloads
the new version;
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

Open a photo or video from your file manager with Ommata, or from a terminal:

```sh
ommata photo.jpg        # open a file, with its folder ready to browse
ommata ~/Pictures/trip  # open a folder
```

Press `F1` (or `Alt+?`) for Help: a search of the keyboard shortcuts, your version and updates, and
reporting a problem. Settings are behind the gear in the top bar, or on the start screen.

### Keyboard shortcuts

These are the defaults. To change a key, open Settings and click it under **Keybinds**.
Keys that type a character take `Alt` (or `Ctrl`), so typing while a file is open starts a
tag in the tag bar.

| Browse        |                    | View                    |                             |
| ------------- | ------------------ | ----------------------- | --------------------------- |
| `←` `→`       | Previous / next    | Scroll, `Alt+=` `Alt+-` | Zoom                        |
| `Home` `End`  | First / last       | `Alt+0` / `Alt+Z`       | Fit to window / actual size |
| `Del`         | Move to trash      | `Ctrl+F`, `F11`         | Fullscreen                  |
| `Ctrl+O`      | Open a file        | `F1`, `Alt+?`           | Help                        |
| `F2`          | Rename             |                         |                             |

| Video            |                           | Photo                 |                            |
| ---------------- | ------------------------- | --------------------- | -------------------------- |
| `Space`          | Play / pause              | `Alt+C`               | Crop                       |
| `Alt+J` `Alt+L`  | Back / forward 5 seconds  | `1` – `5`             | Crop shape, while cropping |
| `Alt+,` `Alt+.`  | Back / forward one frame  | `Enter`               | Apply crop                 |
| `Alt+M`          | Mute                      | `Alt+R` `Alt+Shift+R` | Rotate right / left        |
| `↑` `↓`          | Volume                    | `Alt+H`               | Flip                       |
| `Alt+T`          | Trim                      | `Esc`                 | Cancel crop                |
| `Alt+I` `Alt+O`  | Set trim start / end      |                       |                            |
| `Esc`            | Leave trim                |                       |                            |

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
| `Ctrl+X`       | Clear the rating                           |
| `Backspace`    | Reject                                     |
| `Ctrl+6` – `9` | Red, yellow, green or blue label           |
| `Ctrl+0`       | Purple label                               |
| `Ctrl+K`       | Show or hide the tag bar                   |
| `Ctrl+C`       | Copy the file's tags                       |
| `Ctrl+V`       | Add the copied tags                        |
| `Ctrl+Shift+V` | Replace the file's tags with the copied    |
| `Ctrl+Shift+F` | Filter                                     |
| Type a letter  | Open the tag bar and start typing a tag    |

Edits to a camera RAW save as a DNG: all the original sensor data, with the crop and
rotation stored in the file for RAW editors (Lightroom, darktable, RawTherapee) to apply.
Saving over a RAW writes `name.dng` and moves the original to the trash.

To rename a file, click its name in the top bar or press `F2`, type, and press Enter.

`Del` asks before moving a file to the trash. To clear out a folder quickly, tick
**Don't ask again until Ommata closes**.

## Reporting problems

Press `F1`, open **Report an issue**, and click **Report an issue**. It opens a
[bug report](https://github.com/mmzim05/Ommata/issues/new/choose) with your Ommata
version, system and GPU already filled in; describe what happened and, if you can, which
file it happened with. Ideas are welcome as feature requests.

## Supported files

| Kind       | Formats                                                                                                                           |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------- |
| Photos     | JPEG, PNG, WebP, GIF, TIFF, BMP                                                                                                   |
| Camera RAW | Canon, Nikon, Sony, Fujifilm, Olympus, Panasonic, Pentax, Samsung, Leica and other DNG, Phase One, Kodak, Epson, Minolta          |
| Video      | MP4, MOV, MKV, WebM, AVI, MPEG-TS, MPG, FLV, OGV, WMV, 3GP                                                                        |
| Audio      | MP3, AAC, M4A, FLAC, WAV, Ogg Vorbis, Opus, WMA, AIFF, ALAC, APE, WavPack, TTA, TAK, Musepack, AC-3, DTS, TrueHD, AMR, DSD, more |

Hasselblad (`.3fr`) and Leaf (`.mos`) RAW files do not open yet.

Audio files play with an empty screen for now. APE, Musepack, TAK, Shorten, DSD and QOA
files play but can't be trimmed.

## Build from source

On Arch:

```sh
sudo pacman -S --needed git rust meson ninja nasm libva x264 x265
git clone https://github.com/mmzim05/Ommata
cd Ommata
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
