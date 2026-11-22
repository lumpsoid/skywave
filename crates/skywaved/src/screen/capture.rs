//! Turns a PNG a device captured into the image a session stores (D65): a
//! JPEG scaled to fit [`MAX_SIDE`], or the PNG kept whole for `full`. A
//! capture whose pixels equal the latest one is `Unchanged`, so aims at the
//! latest capture stay valid.

use image::DynamicImage;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use skywave_proto::{CaptureFormat, ContentHash};

use super::backend::BackendError;
use crate::event_log::LatestCapture;

/// The longest side, in pixels, of a stored JPEG capture.
pub const MAX_SIDE: u32 = 1280;
/// The JPEG quality of a stored capture.
pub const JPEG_QUALITY: u8 = 70;

/// A capture ready to store: the bytes, what they are, and the sizes that
/// aims use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// The stored image's bytes.
    pub bytes: Vec<u8>,
    /// The bytes' encoding.
    pub format: CaptureFormat,
    /// The stored image's width in pixels.
    pub width: u32,
    /// The stored image's height in pixels.
    pub height: u32,
    /// The device screen's width in pixels.
    pub screen_width: u32,
    /// The device screen's height in pixels.
    pub screen_height: u32,
    /// The hash of the decoded RGBA8 pixels.
    pub pixels: ContentHash,
}

/// What processing a capture made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Processed {
    /// The pixels equal the latest capture's: the latest one stands.
    Unchanged,
    /// A new capture to store.
    New(Encoded),
}

/// Processes the PNG `png` a device captured. `latest` is the pixels hash of
/// the device's latest capture in the session. Unless `full`, equal pixels
/// are `Unchanged`; `full` always makes a new PNG.
pub fn process(
    png: &[u8],
    full: bool,
    latest: Option<&ContentHash>,
) -> Result<Processed, BackendError> {
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| BackendError(format!("cannot decode the capture: {e}")))?;
    let (width, height) = (decoded.width(), decoded.height());
    let rgba = decoded.to_rgba8();
    let pixels = ContentHash::of(rgba.as_raw());
    if !full && latest == Some(&pixels) {
        return Ok(Processed::Unchanged);
    }
    if full {
        return Ok(Processed::New(Encoded {
            bytes: png.to_vec(),
            format: CaptureFormat::Png,
            width,
            height,
            screen_width: width,
            screen_height: height,
            pixels,
        }));
    }
    let (out_width, out_height) = fit(width, height);
    let scaled = if (out_width, out_height) == (width, height) {
        rgba
    } else {
        imageops::resize(&rgba, out_width, out_height, FilterType::Triangle)
    };
    let rgb = DynamicImage::ImageRgba8(scaled).into_rgb8();
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(rgb)
        .write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY))
        .map_err(|e| BackendError(format!("cannot encode the capture: {e}")))?;
    Ok(Processed::New(Encoded {
        bytes,
        format: CaptureFormat::Jpeg,
        width: out_width,
        height: out_height,
        screen_width: width,
        screen_height: height,
        pixels,
    }))
}

/// The size a `width` x `height` capture is stored at: scaled down so the
/// longest side is at most [`MAX_SIDE`], aspect kept, never up.
fn fit(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= MAX_SIDE {
        return (width, height);
    }
    let scale = |side: u32| {
        let scaled = u64::from(side) * u64::from(MAX_SIDE) / u64::from(longest);
        scaled.max(1) as u32
    };
    (scale(width), scale(height))
}

/// The point on the device screen that the point `(x, y)` of the latest
/// capture's image is at.
pub fn to_screen(x: u32, y: u32, latest: &LatestCapture) -> (u32, u32) {
    let scale = |point: u32, image: u32, screen: u32| {
        (u64::from(point) * u64::from(screen) / u64::from(image)) as u32
    };
    (
        scale(x, latest.width, latest.screen_width),
        scale(y, latest.height, latest.screen_height),
    )
}

#[cfg(test)]
mod tests {
    use image::codecs::png::PngEncoder;
    use image::{Rgba, RgbaImage};
    use skywave_proto::CommandId;

    use super::*;

    /// A solid `width` x `height` PNG.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba([40, 80, 120, 255]));
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_with_encoder(PngEncoder::new(&mut out))
            .unwrap();
        out
    }

    /// The new capture `processed` holds; panics on `Unchanged`.
    fn new_capture(processed: Processed) -> Encoded {
        match processed {
            Processed::New(encoded) => encoded,
            Processed::Unchanged => panic!("expected a new capture"),
        }
    }

    #[test]
    fn scales_a_phone_screen_to_a_jpeg() {
        let encoded = new_capture(process(&png(1080, 2400), false, None).unwrap());
        assert_eq!(encoded.format, CaptureFormat::Jpeg);
        assert_eq!((encoded.width, encoded.height), (576, 1280));
        assert_eq!((encoded.screen_width, encoded.screen_height), (1080, 2400));
        assert!(encoded.bytes.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn an_unchanged_screen_keeps_the_latest_capture() {
        let image = png(1080, 2400);
        let first = new_capture(process(&image, false, None).unwrap());
        let again = process(&image, false, Some(&first.pixels)).unwrap();
        assert_eq!(again, Processed::Unchanged);
    }

    #[test]
    fn a_full_capture_is_a_png_even_when_unchanged() {
        let image = png(1080, 2400);
        let first = new_capture(process(&image, false, None).unwrap());
        let encoded = new_capture(process(&image, true, Some(&first.pixels)).unwrap());
        assert_eq!(encoded.format, CaptureFormat::Png);
        assert_eq!(encoded.bytes, image);
        assert_eq!((encoded.width, encoded.height), (1080, 2400));
    }

    #[test]
    fn a_small_screen_is_not_upscaled() {
        let encoded = new_capture(process(&png(640, 480), false, None).unwrap());
        assert_eq!((encoded.width, encoded.height), (640, 480));
    }

    #[test]
    fn garbage_is_not_a_capture() {
        assert!(process(b"not a png", false, None).is_err());
    }

    #[test]
    fn a_tap_maps_back_to_the_screen() {
        let latest = LatestCapture {
            capture: CommandId::new(),
            pixels: ContentHash::of(b"pixels"),
            width: 576,
            height: 1280,
            screen_width: 1080,
            screen_height: 2400,
        };
        assert_eq!(to_screen(288, 640, &latest), (540, 1200));
    }
}
