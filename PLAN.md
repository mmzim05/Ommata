# omalens: plan

## Goal

A single, fast desktop app for Linux that replaces the separate image viewer,
video player, and quick-edit tools:

- View photos: JPEG, PNG, WebP, AVIF, HEIC, JXL, GIF, and camera RAW
  (CR2/CR3, NEF, ARW, RAF, ORF, RW2, DNG).
- Play videos: anything GStreamer can decode (H.264, H.265, VP9, AV1, ...).
- Browse a folder: thumbnail grid, next/previous, delete, open with.
- Crop and rotate photos.
- Trim videos (cut start/end).

Non-goals: photo library/catalog management, full RAW development
(exposure curves, masks), multi-track video editing. Tools like darktable and
Kdenlive already do those well.

Tagging is now in scope: tags, ratings and colour labels in XMP sidecars that
digiKam, darktable and Lightroom share, so sorting a shoot needs no catalog app.

## Stack

| Concern | Choice | Why |
|---|---|---|
| Language | Rust (stable) | Requested; safe and fast for decoding pipelines |
| UI toolkit | GTK4 + libadwaita via `gtk4-rs` / `libadwaita-rs` | Native Linux look, mature Rust bindings, GPU-rendered textures |
| Video playback | GStreamer via `gstreamer-rs` + `gtk4paintablesink` (`gst-plugin-gtk4`) | Renders straight into a GTK widget, hardware decoding via VA-API |
| Common images | `glycin` (sandboxed loaders, used by GNOME Loupe), fallback `image` crate | Broad format support, decoders run sandboxed |
| RAW decoding | `rawler` (pure Rust, from dnglab) | No C dependency, wide camera support; fall back to embedded JPEG preview for speed |
| Image saving | `image` crate; `turbojpeg` for lossless JPEG crop/rotate on MCU boundaries | Avoids re-encoding JPEGs where possible |
| Metadata | `kamadak-exif` (read), `little_exif` (write) | Orientation, date, camera info panel |
| Video trimming | Lossless: GStreamer pipeline or `ffmpeg -ss/-to -c copy` subprocess. Exact: re-encode | Lossless cuts snap to keyframes; offer "exact" mode that re-encodes |
| Thumbnails | Freedesktop thumbnail cache (`~/.cache/thumbnails`) | Share thumbnails with other apps |
| Packaging | Cargo + Meson-free build; AUR `PKGBUILD`, later Flatpak | Simple for Arch first |

## Architecture

```
src/
  main.rs            app startup, CLI args (open file or folder)
  app/               AdwApplication, window, actions, keybindings
  browser/           folder scanning, file watcher (notify), thumbnail grid
  viewer/
    image_view.rs    zoom/pan canvas, GdkTexture rendering
    video_view.rs    GStreamer playbin + gtk4paintablesink, controls
  decode/
    mod.rs           MediaKind detection (by magic bytes, then extension)
    raster.rs        glycin / image loaders
    raw.rs           rawler: embedded preview first, full decode on demand
  edit/
    crop.rs          crop rectangle overlay, aspect presets, rotate/flip
    trim.rs          in/out markers on the timeline, export job
  export/            save-as / overwrite with backup, background jobs
  meta/              EXIF read/write, info sidebar
```

Rules:

- Decoding runs off the UI thread (`gio` tasks or a small thread pool) and
  sends textures back over a channel. The UI never blocks on I/O.
- Preload next/previous file so arrow-key browsing feels instant.
- Edits never overwrite the original by default: "Save" writes a copy;
  "Overwrite" is an explicit action that keeps a `.bak` until the app closes.

## Milestones

### M0: skeleton
- Cargo project, GTK4/libadwaita window, open a file from CLI or file chooser.
- CI: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on GitHub Actions (Arch container).

### M1: image viewer
- Display common formats, zoom (fit / 100% / scroll), pan, EXIF orientation.
- Folder navigation with arrow keys, preload neighbours.
- Fullscreen, delete-to-trash (`gio::File::trash`).

### M2: RAW
- Show embedded JPEG preview instantly; decode full RAW in background.
- Info sidebar: camera, lens, ISO, shutter, aperture, date.

### M3: video player
- Play/pause, seek bar, volume, frame-step, playback speed.
- Same window and navigation as images: arrow keys move between files of any type.

### M4: crop and rotate
- Drag-to-crop overlay with aspect presets (free, 1:1, 4:3, 3:2, 16:9).
- Rotate 90°, flip. Lossless path for JPEG when the crop allows it.
- RAW: export cropped result as JPEG/PNG/TIFF (never write back to RAW).

### M5: video trim
- Set in/out points on the timeline (I/O keys), preview the range.
- Export: "fast (lossless, keyframe-aligned)" or "exact (re-encode)".
- Progress dialog, cancellable.

### M6: browser and polish
- Thumbnail grid view with freedesktop thumbnail cache.
- Sort by name/date, filter by type.
- Register as default handler (`.desktop` file with image/* and video/* MIME types).
- AUR package.

## Open questions

- GStreamer vs. libmpv for playback: GStreamer integrates cleanly with GTK4;
  libmpv has wider format quirk handling. Start with GStreamer, revisit if
  playback issues show up.
- glycin needs its loader binaries installed system-wide; decide whether to
  depend on it or use the `image` crate alone for M1.
- Lossless JPEG crop: `turbojpeg` crate vs. calling `jpegtran`.

## Prior art to study

- Loupe (GNOME image viewer, Rust + GTK4 + glycin)
- Showtime / Clapper (GTK4 video players on GStreamer)
- LosslessCut (keyframe-aligned trimming UX)
- dnglab / rawler (RAW decoding)
