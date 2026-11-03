//! Still images: background decode with EXIF orientation applied, plus
//! non-destructive rotate/flip/crop that is only baked into pixels on save.

use eframe::egui::{Color32, ColorImage, Pos2, Rect, pos2};
use image::{DynamicImage, ImageDecoder, ImageReader};
use std::path::Path;

pub struct Decoded {
    pub image: ColorImage,
    /// Pixel size of the oriented original (`image` may be downscaled).
    pub size: [usize; 2],
}

/// Oriented pixels plus the raw EXIF and ICC chunks to carry over on save.
type Read = (DynamicImage, Option<Vec<u8>>, Option<Vec<u8>>);

pub fn is_raw(path: &Path) -> bool {
    crate::library::RAW_EXT.contains(&ext(path).as_str())
}

/// Camera RAW. For display, the camera's embedded JPEG preview is used: it
/// decodes in milliseconds instead of a full demosaic. `full` develops the
/// sensor data (for saving).
fn read_raw(path: &Path, full: bool) -> Result<Read, String> {
    use rawler::decoders::RawDecodeParams;
    let err = |e: rawler::RawlerError| e.to_string();
    let src = rawler::rawsource::RawSource::new(path).map_err(|e| e.to_string())?;
    let dec = rawler::get_decoder(&src).map_err(err)?;
    let params = RawDecodeParams::default();
    let embedded = || {
        dec.full_image(&src, &params)
            .ok()
            .flatten()
            .or_else(|| dec.preview_image(&src, &params).ok().flatten())
    };
    let develop = || -> Result<DynamicImage, String> {
        let raw = dec.raw_image(&src, &params, false).map_err(err)?;
        rawler::imgop::develop::RawDevelop::default()
            .develop_intermediate(&raw)
            .map_err(err)?
            .to_dynamic_image()
            .ok_or_else(|| "unsupported RAW layout".to_string())
    };
    let mut img = match (full, embedded()) {
        (false, Some(img)) => img,
        _ => develop()?,
    };
    let orientation = dec
        .raw_metadata(&src, &params)
        .ok()
        .and_then(|m| m.exif.orientation)
        .and_then(|o| image::metadata::Orientation::from_exif(o as u8));
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok((img, None, None))
}

fn read(path: &Path, full: bool) -> Result<Read, String> {
    if is_raw(path) {
        return read_raw(path, full);
    }
    let reader = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?;
    let mut dec = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = dec
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let exif = dec.exif_metadata().ok().flatten();
    let icc = dec.icc_profile().ok().flatten();
    let mut img = DynamicImage::from_decoder(dec).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok((img, exif, icc))
}

/// Decode for display, downscaled to fit `max_side` (the GPU texture limit).
pub fn decode(path: &Path, max_side: u32) -> Result<Decoded, String> {
    let (img, _, _) = read(path, false)?;
    let size = [img.width() as usize, img.height() as usize];
    let img = if img.width() > max_side || img.height() > max_side {
        img.resize(max_side, max_side, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    Ok(Decoded {
        image: to_color_image(img),
        size,
    })
}

/// Convert for upload in one pass spread over all cores (opaque RGB, the JPEG
/// and RAW case, needs no premultiply); other layouts go through RGBA.
fn to_color_image(img: DynamicImage) -> ColorImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let DynamicImage::ImageRgb8(rgb) = img else {
        let rgba = img.into_rgba8();
        return ColorImage::from_rgba_unmultiplied([w, h], &rgba);
    };
    let mut pixels = vec![Color32::BLACK; w * h];
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let rows = h.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for (dst, src) in pixels
            .chunks_mut(rows * w)
            .zip(rgb.as_raw().chunks(rows * w * 3))
        {
            s.spawn(move || {
                for (d, p) in dst.iter_mut().zip(src.as_chunks::<3>().0) {
                    *d = Color32::from_rgb(p[0], p[1], p[2]);
                }
            });
        }
    });
    ColorImage::new([w, h], pixels)
}

/// File-manager thumbnail: `size` px on the long side, written as PNG. RAW
/// files use the camera's embedded preview, so this stays fast.
pub fn thumbnail(path: &Path, out: &Path, size: u32) -> Result<(), String> {
    let (img, _, _) = read(path, false)?;
    let img = img.thumbnail(size, size);
    let img = if img.color().has_alpha() {
        DynamicImage::ImageRgba8(img.into_rgba8())
    } else {
        DynamicImage::ImageRgb8(img.into_rgb8())
    };
    img.save_with_format(out, image::ImageFormat::Png)
        .map_err(|e| e.to_string())
}

/// Rotation (quarter turns clockwise) then horizontal flip, then crop. The crop
/// rect is normalized to the rotated/flipped image.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub struct Edit {
    pub rot: u8,
    pub flip: bool,
    pub crop: Option<Rect>,
}

impl Edit {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    pub fn rotate(&mut self, clockwise: bool) {
        // Flip conjugates rotation: R∘F = F∘R⁻¹.
        let cw = clockwise != self.flip;
        self.rot = (self.rot + if cw { 1 } else { 3 }) % 4;
        self.crop = self.crop.map(|r| {
            let (a, b) = if clockwise {
                (pos2(1.0 - r.max.y, r.min.x), pos2(1.0 - r.min.y, r.max.x))
            } else {
                (pos2(r.min.y, 1.0 - r.max.x), pos2(r.max.y, 1.0 - r.min.x))
            };
            Rect::from_two_pos(a, b)
        });
    }

    pub fn flip_h(&mut self) {
        self.flip = !self.flip;
        self.crop = self.crop.map(|r| {
            Rect::from_two_pos(pos2(1.0 - r.max.x, r.min.y), pos2(1.0 - r.min.x, r.max.y))
        });
    }

    /// Size after rotation, before crop.
    pub fn rotated_size(&self, size: [usize; 2]) -> [usize; 2] {
        if self.rot % 2 == 1 {
            [size[1], size[0]]
        } else {
            size
        }
    }

    /// Size of the final image.
    pub fn output_size(&self, size: [usize; 2]) -> [usize; 2] {
        let s = self.rotated_size(size);
        match self.crop {
            Some(c) => [
                ((c.width() * s[0] as f32).round() as usize).max(1),
                ((c.height() * s[1] as f32).round() as usize).max(1),
            ],
            None => s,
        }
    }

    /// Map a normalized point of the edited (rotated/flipped) image to texture UV.
    pub fn to_uv(self, mut p: Pos2) -> Pos2 {
        if self.flip {
            p.x = 1.0 - p.x;
        }
        for _ in 0..self.rot {
            p = pos2(p.y, 1.0 - p.x);
        }
        p
    }

    pub fn apply(&self, mut img: DynamicImage) -> DynamicImage {
        img = match self.rot {
            1 => img.rotate90(),
            2 => img.rotate180(),
            3 => img.rotate270(),
            _ => img,
        };
        if self.flip {
            img = img.fliph();
        }
        if let Some(c) = self.crop {
            let (w, h) = (img.width() as f32, img.height() as f32);
            let x = (c.min.x * w).round().clamp(0.0, w - 1.0) as u32;
            let y = (c.min.y * h).round().clamp(0.0, h - 1.0) as u32;
            let cw = ((c.width() * w).round() as u32).clamp(1, img.width() - x);
            let ch = ((c.height() * h).round() as u32).clamp(1, img.height() - y);
            img = img.crop_imm(x, y, cw, ch);
        }
        img
    }
}

/// Formats we can write back in their own format.
pub fn can_save(path: &Path) -> bool {
    matches!(
        ext(path).as_str(),
        "jpg" | "jpeg" | "jpe" | "png" | "webp" | "tif" | "tiff" | "bmp"
    ) || is_raw(path)
}

fn ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Re-decode the full-resolution original, apply `edit`, and encode to `dst`
/// in the source's format (RAW develops to JPEG), keeping EXIF (orientation reset, since it is baked
/// into the pixels now) and the ICC profile.
pub fn save(src: &Path, dst: &Path, edit: &Edit) -> Result<(), String> {
    use image::ImageEncoder;
    let (img, exif, icc) = read(src, true)?;
    let img = edit.apply(img);
    let mut exif = exif;
    if let Some(e) = &mut exif {
        let _ = image::metadata::Orientation::remove_from_exif_chunk(e);
    }
    let file = std::fs::File::create(dst).map_err(|e| e.to_string())?;
    let mut w = std::io::BufWriter::new(file);
    let format = if is_raw(src) { "jpg".into() } else { ext(src) };
    let res = match format.as_str() {
        "jpg" | "jpeg" | "jpe" => {
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, 95);
            if let Some(e) = exif {
                let _ = enc.set_exif_metadata(e);
            }
            if let Some(p) = icc {
                let _ = enc.set_icc_profile(p);
            }
            let img = if img.color().has_alpha() || img.color().bytes_per_pixel() > 3 {
                DynamicImage::ImageRgb8(img.into_rgb8())
            } else {
                img
            };
            img.write_with_encoder(enc)
        }
        "png" => {
            let mut enc = image::codecs::png::PngEncoder::new(&mut w);
            if let Some(e) = exif {
                let _ = enc.set_exif_metadata(e);
            }
            if let Some(p) = icc {
                let _ = enc.set_icc_profile(p);
            }
            img.write_with_encoder(enc)
        }
        "webp" => {
            let mut enc = image::codecs::webp::WebPEncoder::new_lossless(&mut w);
            if let Some(e) = exif {
                let _ = enc.set_exif_metadata(e);
            }
            if let Some(p) = icc {
                let _ = enc.set_icc_profile(p);
            }
            let img = if img.color().has_alpha() {
                DynamicImage::ImageRgba8(img.into_rgba8())
            } else {
                DynamicImage::ImageRgb8(img.into_rgb8())
            };
            img.write_with_encoder(enc)
        }
        "tif" | "tiff" => img.write_to(&mut w, image::ImageFormat::Tiff),
        "bmp" => img.write_to(&mut w, image::ImageFormat::Bmp),
        other => return Err(format!("can't save .{other} files")),
    };
    res.map_err(|e| e.to_string())?;
    let file = w.into_inner().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_four_times_is_identity() {
        let mut e = Edit {
            crop: Some(Rect::from_min_max(pos2(0.1, 0.2), pos2(0.5, 0.9))),
            ..Default::default()
        };
        let orig = e;
        for _ in 0..4 {
            e.rotate(true);
        }
        assert_eq!(e.rot, 0);
        let c = e.crop.unwrap();
        assert!((c.min - orig.crop.unwrap().min).length() < 1e-6);
    }

    #[test]
    fn uv_matches_pixels() {
        // 3x2 image with distinct pixels; the edited image's pixel at each
        // normalized position must equal the source pixel at to_uv().
        let src = image::RgbImage::from_fn(3, 2, |x, y| image::Rgb([x as u8, y as u8, 0]));
        for rot in 0..4 {
            for flip in [false, true] {
                let e = Edit {
                    rot,
                    flip,
                    crop: None,
                };
                let out = e.apply(DynamicImage::ImageRgb8(src.clone())).into_rgb8();
                for (x, y, px) in out.enumerate_pixels() {
                    let p = pos2(
                        (x as f32 + 0.5) / out.width() as f32,
                        (y as f32 + 0.5) / out.height() as f32,
                    );
                    let uv = e.to_uv(p);
                    let sx = (uv.x * 3.0) as u8;
                    let sy = (uv.y * 2.0) as u8;
                    assert_eq!(px.0, [sx, sy, 0], "rot {rot} flip {flip} at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn rotate_with_flip_matches_pixels() {
        let src = image::RgbImage::from_fn(3, 2, |x, y| image::Rgb([x as u8, y as u8, 0]));
        let mut e = Edit::default();
        e.flip_h();
        e.rotate(true);
        // Expected: flip the source, then rotate the result clockwise.
        let expect = DynamicImage::ImageRgb8(src.clone())
            .fliph()
            .rotate90()
            .into_rgb8();
        assert_eq!(e.apply(DynamicImage::ImageRgb8(src)).into_rgb8(), expect);
    }
}

#[cfg(test)]
mod raw_tests {
    /// Display decode time for any image: ANYVIEW_TEST_IMAGE=x.jpg cargo test image_decode -- --ignored
    #[test]
    #[ignore]
    fn image_decode_time() {
        let p = std::path::PathBuf::from(std::env::var("ANYVIEW_TEST_IMAGE").unwrap());
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let d = super::decode(&p, 8192).unwrap();
            eprintln!("decode {:?} in {:?}", d.size, t.elapsed());
        }
    }

    /// Needs a real RAW file: ANYVIEW_TEST_RAW=photo.rw2 cargo test -- --ignored
    #[test]
    #[ignore]
    fn raw_preview_and_develop() {
        let p = std::path::PathBuf::from(std::env::var("ANYVIEW_TEST_RAW").unwrap());
        let t = std::time::Instant::now();
        let d = super::decode(&p, 8192).unwrap();
        eprintln!("preview {:?} in {:?}", d.size, t.elapsed());
        let t = std::time::Instant::now();
        let (full, _, _) = super::read(&p, true).unwrap();
        eprintln!(
            "develop {}x{} in {:?}",
            full.width(),
            full.height(),
            t.elapsed()
        );
    }
}
