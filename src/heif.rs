//! HEIF stills (HEIC from phones) through the bundled FFmpeg: its mov
//! demuxer splits the file into one stream per tile plus a tile grid, the
//! HEVC (or AV1) decoder decodes the tiles, and the grid is assembled here.

use ffmpeg_next as ff;
use image::metadata::Orientation;
use image::{DynamicImage, RgbImage};
use std::path::Path;

/// Where a decoded tile goes on the canvas.
struct Tile {
    stream: usize,
    left: u32,
    top: u32,
}

/// The primary image's layout: its tiles on a canvas, the crop shown from
/// that canvas, and side data (orientation, EXIF).
struct Layout {
    tiles: Vec<Tile>,
    canvas: [u32; 2],
    /// Left, top, width, height.
    crop: [u32; 4],
    side_data: *const ff::ffi::AVPacketSideData,
    side_data_count: i32,
}

/// Decode the primary image of the HEIF file at `path`, oriented, with its
/// EXIF (orientation already stripped by FFmpeg), in sRGB.
pub fn read(path: &Path) -> Result<crate::picture::Read, String> {
    let mut input = ff::format::input(path).map_err(|error| error.to_string())?;
    let layout = layout(&input).ok_or("no image in this HEIF file")?;
    let mut canvas = RgbImage::new(layout.canvas[0], layout.canvas[1]);
    let mut decoders = Vec::new();
    for tile in &layout.tiles {
        if decoders.iter().any(|(index, _)| *index == tile.stream) {
            continue;
        }
        let stream = input.stream(tile.stream).ok_or("missing tile stream")?;
        let decoder = ff::codec::Context::from_parameters(stream.parameters())
            .and_then(|codec| codec.decoder().video())
            .map_err(|error| error.to_string())?;
        decoders.push((tile.stream, decoder));
    }
    let mut scaler = crate::video::Scaler::new().ok_or("can't allocate a pixel converter")?;
    let (mut frame, mut rgba) = (ff::frame::Video::empty(), ff::frame::Video::empty());
    let mut place = |index: usize, frame: &ff::frame::Video| -> Result<(), String> {
        if !scaler.run(frame, &mut rgba, frame.width(), frame.height()) {
            return Err("can't convert a tile's pixels".into());
        }
        for tile in layout.tiles.iter().filter(|tile| tile.stream == index) {
            blit(&mut canvas, &rgba, tile.left, tile.top);
        }
        Ok(())
    };
    // Each tile stream holds one picture.
    for (stream, packet) in input.packets() {
        let Some((index, decoder)) = decoders
            .iter_mut()
            .find(|(index, _)| *index == stream.index())
        else {
            continue;
        };
        decoder
            .send_packet(&packet)
            .map_err(|error| error.to_string())?;
        decoder.send_eof().map_err(|error| error.to_string())?;
        decoder
            .receive_frame(&mut frame)
            .map_err(|error| error.to_string())?;
        place(*index, &frame)?;
    }
    let [left, top, width, height] = layout.crop;
    let mut picture = DynamicImage::ImageRgb8(canvas).crop_imm(left, top, width, height);
    // SAFETY: the side data belongs to `input`, which is still open.
    let (orientation, exif) = unsafe {
        let side = |kind| side_data(layout.side_data, layout.side_data_count, kind);
        (
            side(ff::ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX)
                .and_then(|matrix| orientation(&matrix)),
            side(ff::ffi::AVPacketSideDataType::AV_PKT_DATA_EXIF),
        )
    };
    if let Some(orientation) = orientation {
        picture.apply_orientation(orientation);
    }
    // No ICC profile: swscale already converted the tiles from their
    // primaries (Display P3 on iPhones) to sRGB, which the profile would undo.
    Ok((picture, exif, None))
}

/// The primary image: the tile grid FFmpeg marks as default (phone photos
/// are grids of 512 px tiles), else the default single-picture stream.
fn layout(input: &ff::format::context::Input) -> Option<Layout> {
    let default = ff::ffi::AV_DISPOSITION_DEFAULT;
    // SAFETY: reads the demuxer's stream groups, which live as long as
    // `input`; FFmpeg checks each grid's offsets against its canvas.
    unsafe {
        let context = input.as_ptr();
        let groups = std::slice::from_raw_parts(
            (*context).stream_groups,
            (*context).nb_stream_groups as usize,
        );
        let grids = groups.iter().map(|&group| &*group).filter(|group| {
            group.type_ == ff::ffi::AVStreamGroupParamsType::AV_STREAM_GROUP_PARAMS_TILE_GRID
        });
        let grid = grids.clone().find(|group| group.disposition & default != 0);
        if let Some(group) = grid.or_else(|| grids.clone().next()) {
            let grid = &*group.params.tile_grid;
            let streams = std::slice::from_raw_parts(group.streams, group.nb_streams as usize);
            let offsets = std::slice::from_raw_parts(grid.offsets, grid.nb_tiles as usize);
            let tiles = offsets
                .iter()
                .map(|offset| Tile {
                    stream: (*streams[offset.idx as usize]).index as usize,
                    left: offset.horizontal as u32,
                    top: offset.vertical as u32,
                })
                .collect();
            return Some(Layout {
                tiles,
                canvas: [grid.coded_width as u32, grid.coded_height as u32],
                crop: [
                    grid.horizontal_offset as u32,
                    grid.vertical_offset as u32,
                    grid.width as u32,
                    grid.height as u32,
                ],
                side_data: grid.coded_side_data,
                side_data_count: grid.nb_coded_side_data,
            });
        }
    }
    let stream = input
        .streams()
        .find(|stream| {
            stream.parameters().medium() == ff::media::Type::Video
                && stream
                    .disposition()
                    .contains(ff::format::stream::Disposition::DEFAULT)
        })
        .or_else(|| input.streams().best(ff::media::Type::Video))?;
    // SAFETY: the codec parameters live as long as `input`.
    let parameters = unsafe { &*(*stream.as_ptr()).codecpar };
    let (width, height) = (parameters.width as u32, parameters.height as u32);
    Some(Layout {
        tiles: vec![Tile {
            stream: stream.index(),
            left: 0,
            top: 0,
        }],
        canvas: [width, height],
        crop: [0, 0, width, height],
        side_data: parameters.coded_side_data,
        side_data_count: parameters.nb_coded_side_data,
    })
}

/// Copy an RGBA frame into `canvas` at `left`, `top`, clipped to the canvas.
fn blit(canvas: &mut RgbImage, rgba: &ff::frame::Video, left: u32, top: u32) {
    let (canvas_width, canvas_height) = canvas.dimensions();
    let width = rgba.width().min(canvas_width.saturating_sub(left)) as usize;
    let height = rgba.height().min(canvas_height.saturating_sub(top));
    let stride = rgba.stride(0);
    let row_bytes = canvas_width as usize * 3;
    let pixels: &mut [u8] = canvas;
    for (row, src) in rgba
        .data(0)
        .chunks(stride)
        .take(height as usize)
        .enumerate()
    {
        let start = (top as usize + row) * row_bytes + left as usize * 3;
        let dst = &mut pixels[start..start + width * 3];
        for (dst, src) in dst
            .as_chunks_mut::<3>()
            .0
            .iter_mut()
            .zip(src.as_chunks::<4>().0)
        {
            *dst = [src[0], src[1], src[2]];
        }
    }
}

/// A copy of the side data of `kind`, if present.
///
/// # Safety
/// `data` must point to `count` valid entries.
unsafe fn side_data(
    data: *const ff::ffi::AVPacketSideData,
    count: i32,
    kind: ff::ffi::AVPacketSideDataType,
) -> Option<Vec<u8>> {
    // SAFETY: guaranteed by the caller; each entry's data holds `size` bytes.
    unsafe {
        let entry = ff::ffi::av_packet_side_data_get(data, count, kind);
        if entry.is_null() || (*entry).data.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts((*entry).data, (*entry).size).to_vec())
    }
}

/// How to turn the decoded picture for display (HEIF `irot` and `imir`),
/// read from a display matrix the way FFmpeg's own players read it.
fn orientation(matrix: &[u8]) -> Option<Orientation> {
    let values: Vec<i32> = matrix
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| i32::from_ne_bytes(*bytes))
        .collect();
    if values.len() < 9 {
        return None;
    }
    // SAFETY: `values` holds the nine entries of a display matrix.
    let counter_clockwise = unsafe { ff::ffi::av_display_rotation_get(values.as_ptr()) };
    if !counter_clockwise.is_finite() {
        return None;
    }
    let clockwise = (-counter_clockwise / 90.0).round() as i64;
    Some(match clockwise.rem_euclid(4) {
        1 if values[3] > 0 => Orientation::Rotate90FlipH,
        1 => Orientation::Rotate90,
        2 => match (values[0] < 0, values[4] < 0) {
            (true, true) => Orientation::Rotate180,
            (true, false) => Orientation::FlipHorizontal,
            (false, true) => Orientation::FlipVertical,
            (false, false) => Orientation::NoTransforms,
        },
        3 if values[3] < 0 => Orientation::Rotate270FlipH,
        3 => Orientation::Rotate270,
        _ if values[4] < 0 => Orientation::FlipVertical,
        _ => Orientation::NoTransforms,
    })
}

#[cfg(test)]
mod tests {
    /// A 300×200 picture (red top-left, blue bottom-right) cut into 128 px
    /// tiles and stored with an `irot` that turns it a quarter clockwise.
    #[test]
    fn assembles_tiles_and_turns_upright() {
        let (picture, _, icc) =
            super::read("testdata/heif/grid-rotated.heic".as_ref()).expect("the fixture decodes");
        let picture = picture.into_rgb8();
        assert_eq!(picture.dimensions(), (200, 300));
        let is = |x, y, [red, green, blue]: [u8; 3]| {
            let pixel = picture.get_pixel(x, y).0;
            pixel[0].abs_diff(red) < 16
                && pixel[1].abs_diff(green) < 16
                && pixel[2].abs_diff(blue) < 16
        };
        assert!(is(5, 5, [255, 255, 255]));
        assert!(is(194, 5, [255, 0, 0]));
        assert!(is(5, 294, [0, 0, 255]));
        assert!(icc.is_none());
    }
}
