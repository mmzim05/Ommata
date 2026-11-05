# Changelog

All notable changes to Ommata. Versions follow [Semantic Versioning](https://semver.org):
PATCH for fixes, MINOR for new features or changed behaviour, MAJOR for changes that
break existing use once 1.0 is reached.

Add each change under **Unreleased** as it lands; `cargo release` turns that heading into
the new version.

## [Unreleased]

### Changed

- Renamed from omalens to Ommata: the command, package, menu entry, icon, config folder
  (`~/.config/ommata`) and GitHub repository (`mmzim05/Ommata`; old links redirect).
  Settings saved under `~/.config/omalens` are still read, and sidecars omalens wrote
  still count as its own. AppImages from 0.3.1 and earlier don't find the renamed
  release, so download this version by hand once.
- Builds use pinned FFmpeg 9.0.2 and dav1d 1.5.4 release tarballs, checked by SHA-256;
  `FFMPEG_SOURCE` and `DAV1D_SOURCE` build offline from unpacked sources, and
  `scripts/install.sh` installs into `DESTDIR`, for distribution packages.
- Installed as a system package, the Version page says so and leaves updates to the
  system.

## [0.3.1] - 2026-11-05

### Fixed

- AV1 film grain is drawn by omalens's own shader, matching dav1d bit for bit, so
  videos with grain decode on the GPU with every driver. 0.3.0 sent them to the CPU on
  NVIDIA, whose VA-API driver turns grained frames black.

## [0.3.0] - 2026-11-05

### Added

- Tags, star ratings, reject and colour labels for photos and videos, stored in XMP
  sidecars (`name.ext.xmp`, or Lightroom's `name.xmp` when present) that digiKam,
  darktable, Lightroom and exiftool read too. Other apps' data in a sidecar is kept as
  it was. Sidecars follow their file when it is renamed, saved as DNG or trashed.
- Quick tags on `Alt+1` to `Alt+9` (set in Settings), ratings on `Ctrl+0` to `Ctrl+5`,
  reject on `X`, labels on `Ctrl+6` to `Ctrl+9`.
- A tag panel (`Ctrl+K`, or the tag button) and a filter (`Ctrl+F`) that limits browsing
  to files with chosen tags, a minimum rating, a label, or only rejected files.
- Keyboard focus is visible on stars, colour swatches and icon buttons.

### Fixed

- AV1 videos with film grain flickered to black on NVIDIA GPUs, whose VA-API driver
  can't apply the grain. They now decode on the CPU there, grain included.
- Waveform bars on the seek bar are all the same width on scaled displays.

## [0.2.0] - 2026-11-05

### Added

- In-app updates for the AppImage: omalens checks for a new release once a day and
  installs it with one click. Releases also carry `.zsync` update data for Gear Lever and
  AppImageUpdate.
- Help (`?`) with Keybinds, Version (updates) and Report an issue, which opens a GitHub
  bug report with the version and system details filled in.
- Settings in one place for photos and videos, also reachable from the start screen.
- Rename the open file by clicking its name or pressing `F2`.
- Cropped or rotated camera RAW files save as DNG, keeping the full sensor data, instead
  of a JPEG; saving over the original is allowed (it goes to the trash).
- Moving a file to the trash asks first, with an option to stop asking until omalens closes.
- `omalens --version`.

### Changed

- Trimming MP4 and MOV videos is frame-exact in every codec and no longer re-encodes
  anything: saving takes well under half a second instead of several seconds.
- Interface polish: buttons show hover and the pointing hand, the top bar wraps onto two
  rows in a narrow window, dragging a photo stops at its edges, zooming out stops just past
  the window size, the fullscreen button shows its state, and video times show as `m:ss`.
- The red used for trash and discard is readable in every Omarchy theme.

### Fixed

- Large photos (100 MP and up) open several times faster, and photos preloaded in the
  background are no longer limited to 2048 pixels.
- Save progress no longer jumps backwards while trimming.
- Trimming MOV files with 24-bit PCM audio (Blackmagic, Resolve exports) failed.
- The AppImage now carries its icon, so AppImage installers and menus show it.

## [0.1.0] - 2026-11-05

First release.

### Added

- Photos (JPEG, PNG, WebP, TIFF, BMP, GIF) and camera RAW files, with EXIF orientation.
- Video playback with zero-copy VA-API decoding (H.264, HEVC, VP9, AV1) and seek-bar
  thumbnails.
- Lossless, frame-exact video trimming.
- Crop, rotate and flip for photos, saved in place or as a copy.
- Folder browsing with the arrow keys.
- Omarchy theme support.
- Keyboard shortcuts popup (`?`).
- File-manager thumbnailer.
- AppImage release build.

[Unreleased]: https://github.com/mmzim05/Ommata/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/mmzim05/Ommata/releases/tag/v0.3.1
[0.3.0]: https://github.com/mmzim05/Ommata/releases/tag/v0.3.0
[0.2.0]: https://github.com/mmzim05/Ommata/releases/tag/v0.2.0
[0.1.0]: https://github.com/mmzim05/Ommata/releases/tag/v0.1.0
