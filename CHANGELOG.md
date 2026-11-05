# Changelog

All notable changes to omalens. Versions follow [Semantic Versioning](https://semver.org):
PATCH for fixes, MINOR for new features or changed behaviour, MAJOR for changes that
break existing use once 1.0 is reached.

Add each change under **Unreleased** as it lands; `cargo release` turns that heading into
the new version.

## [Unreleased]

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

[Unreleased]: https://github.com/mmzim05/omalens/commits/main
