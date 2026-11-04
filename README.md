# anyview

One fast Linux app for viewing photos and videos, with non-destructive crop,
rotate and trim. Ships as a single executable: FFmpeg is linked statically, the
UI is GPU-rendered egui, and colours follow the active Omarchy theme.

Videos decode on the GPU through VA-API and stay there: frames are imported
into OpenGL as DMA-BUFs and converted to RGB by a shader, never copied
through the CPU. This needs libva and the GPU's VA-API driver (Intel:
`intel-media-driver`, AMD: `libva-mesa-driver`) and an EGL session (Wayland).
Without them, video decodes on the CPU.

## Build

```sh
sudo pacman -S --needed nasm libva  # SIMD for FFmpeg's decoders, VA-API
./scripts/build-ffmpeg.sh           # minimal static FFmpeg into target/ffmpeg
cargo build --release               # target/release/anyview
```

Development against the system FFmpeg (dynamic, no script needed):

```sh
FFMPEG_DIR=/usr cargo run --no-default-features -- <file-or-folder>
```

## Editing

Edits (crop, rotate, flip, trim) live in memory until you save:

- **Save** (`Ctrl+S`) replaces the original; the original goes to the trash as
  `name-original.ext`, so it can be restored.
- **Save copy** (`Ctrl+Shift+S`) writes `name-edit.ext` next to it
  (`name-edit-2.ext`, … if that exists).
- **Revert** (`Ctrl+Z`) drops the edits.

Video trims are lossless (no re-encode); the start snaps to the keyframe at or
before the in point, and the timeline shows that snapped point.

## Keys

| Key | Action |
|---|---|
| `←` `→` / `PgUp` `PgDn` | Previous / next file |
| `Space` | Play / pause |
| `Shift+←` `Shift+→`, `J` `L` | Seek ±5 s |
| `.` / `,` | Step one frame forward / back |
| `M`, `↑` `↓` | Mute, volume |
| `C` | Crop (`Enter` apply, `Esc` cancel) |
| `R` / `Shift+R`, `H` | Rotate right / left, flip |
| `T`, `I` / `O` | Trim mode, set in / out |
| `0`, `1`, scroll, `+` `-` | Fit, 100 %, zoom |
| `F` / `F11` / double-click | Fullscreen |
| `Del` | Move to trash |
| `Ctrl+O` | Open |

Settings (seek-bar waveform style, min/max height, smoothing) are in the gear
menu on the video bar and saved to `~/.config/anyview/settings.conf`.

## License

GPL-3.0-or-later. You may use, change and share anyview; versions you
distribute, modified or not, must stay under the same license with their
source available. See [LICENSE](LICENSE).
