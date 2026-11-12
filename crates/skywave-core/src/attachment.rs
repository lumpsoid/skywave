//! Preparing an image a user attaches to a message, and storing the result
//! under the data dir by its sha256.
//!
//! What gets uploaded is the prepared image, not the picked file: shrunk so
//! the long side is at most [`MAX_SIDE`], and re-encoded so that what the
//! file carried (EXIF GPS, a device serial) never leaves the device. The
//! rules, in order:
//!
//! - The type is sniffed from the bytes ([`ImageType::sniff`]). Anything but
//!   PNG, JPEG, GIF or WEBP is [`AttachRefusal::NotAnImage`].
//! - A GIF passes through unchanged when it is under the cap: resizing an
//!   animated GIF would turn it into a still.
//! - Anything else is decoded, a JPEG's EXIF orientation is applied, and the
//!   image is shrunk to [`MAX_SIDE`] (Lanczos3, aspect kept, never up).
//! - A PNG stays PNG. Its original bytes are kept when it was not shrunk and
//!   they are no larger than the re-encode; those bytes keep any ancillary
//!   chunks the re-encode drops. A JPEG or WEBP becomes a JPEG at
//!   [`JPEG_QUALITY`] with its alpha flattened onto white: the `image` crate
//!   has no lossy WEBP encoder, and a JPEG always leaves re-encoded.
//! - A result over [`ATTACHMENT_MAX_BYTES`] is [`AttachRefusal::TooLarge`].
//!
//! [`store`] writes the prepared bytes to `<data>/attachments/<sha256 hex>`.
//! It lives in core so the TUI's future `/attach <path>` follows the same
//! rules.

use std::fmt;
use std::fs;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::{self, FilterType};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Rgba, RgbaImage};
use skywave_proto::limits::ATTACHMENT_MAX_BYTES;
use skywave_proto::{ContentHash, ImageType};
use uuid::Uuid;

use crate::identity::{sync_dir, write_private};

/// The longest side, in pixels, an attached image keeps.
pub const MAX_SIDE: u32 = 1568;
/// The quality of the JPEG a JPEG or WEBP attachment becomes.
pub const JPEG_QUALITY: u8 = 85;

/// An image ready to upload: the bytes that will be sent, and what they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// What is uploaded and stored.
    pub bytes: Vec<u8>,
    /// The type of `bytes`, which may differ from the picked file's.
    pub media_type: ImageType,
    /// The hash of `bytes`: the file's name in the attachment store.
    pub sha256: ContentHash,
    /// `bytes.len()`.
    pub size: u64,
    /// The width of the image in `bytes`.
    pub width: u32,
    /// The height of the image in `bytes`.
    pub height: u32,
}

impl Prepared {
    /// The image this prepared one is, as a draft keeps it.
    pub fn attached(&self) -> AttachedImage {
        AttachedImage {
            sha256: self.sha256,
            media_type: self.media_type,
            size: self.size,
            width: self.width,
            height: self.height,
        }
    }
}

/// An image in a draft: what the store and the model keep of a prepared
/// image; its bytes are the file at [`attachment_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedImage {
    pub sha256: ContentHash,
    pub media_type: ImageType,
    pub size: u64,
    pub width: u32,
    pub height: u32,
}

/// Why an image cannot be attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachRefusal {
    /// Not PNG, JPEG, GIF or WEBP.
    NotAnImage,
    /// Over [`ATTACHMENT_MAX_BYTES`] still; `bytes` is its size then.
    TooLarge { bytes: u64 },
    /// Of a supported type, but it does not decode (or, not expected, does
    /// not re-encode).
    Unreadable,
}

impl fmt::Display for AttachRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnImage => f.write_str("not an image"),
            Self::TooLarge { bytes } => write!(
                f,
                "image too large ({:.1} MB; at most {:.2} MB)",
                *bytes as f64 / 1_000_000.0,
                ATTACHMENT_MAX_BYTES as f64 / 1_000_000.0
            ),
            Self::Unreadable => f.write_str("the image could not be read"),
        }
    }
}

impl std::error::Error for AttachRefusal {}

/// Prepares the picked or pasted image `bytes` for upload; the module docs
/// give the rules.
pub fn prepare(bytes: &[u8]) -> Result<Prepared, AttachRefusal> {
    let sniffed = ImageType::sniff(bytes).ok_or(AttachRefusal::NotAnImage)?;
    let (media_type, width, height, out) = match sniffed {
        // Animated GIFs pass through whole, so the header is all that is read.
        ImageType::Gif => {
            ensure_within_cap(bytes.len())?;
            let (width, height) = gif_dimensions(bytes)?;
            (ImageType::Gif, width, height, bytes.to_vec())
        }
        ImageType::Png | ImageType::Jpeg | ImageType::Webp => {
            let decoded = decode(bytes, sniffed)?;
            let shrink = decoded.width().max(decoded.height()) > MAX_SIDE;
            let image = if shrink {
                decoded.resize(MAX_SIDE, MAX_SIDE, FilterType::Lanczos3)
            } else {
                decoded
            };
            let (media_type, out) = match sniffed {
                ImageType::Png => {
                    let reencoded = encode_png(&image)?;
                    let original = !shrink && bytes.len() <= reencoded.len();
                    (
                        ImageType::Png,
                        if original { bytes.to_vec() } else { reencoded },
                    )
                }
                _ => (ImageType::Jpeg, encode_jpeg(&image)?),
            };
            (media_type, image.width(), image.height(), out)
        }
    };
    ensure_within_cap(out.len())?;
    Ok(Prepared {
        sha256: ContentHash::of(&out),
        size: out.len() as u64,
        bytes: out,
        media_type,
        width,
        height,
    })
}

/// Where the attachments are kept under `data_dir`.
pub fn attachment_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("attachments")
}

/// Where the attachment with `sha256` is kept under `data_dir`.
pub fn attachment_path(data_dir: &Path, sha256: ContentHash) -> PathBuf {
    attachment_dir(data_dir).join(sha256.to_hex())
}

/// Stores `prepared` under `data_dir` by its sha256 and returns its path. A
/// file already there is left alone: its name is its content's hash. A new
/// one is written whole or not at all: to a temporary file, flushed, renamed
/// into place, and the rename flushed.
pub fn store(data_dir: &Path, prepared: &Prepared) -> io::Result<PathBuf> {
    let dir = attachment_dir(data_dir);
    fs::create_dir_all(&dir)?;
    let path = attachment_path(data_dir, prepared.sha256);
    if path.exists() {
        return Ok(path);
    }
    let temp = dir.join(format!(".tmp-{}", Uuid::new_v4()));
    let written = write_private(&temp, &prepared.bytes).and_then(|()| fs::rename(&temp, &path));
    if let Err(err) = written {
        // Best effort: the write's error is the one the caller needs.
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    sync_dir(&dir)?;
    Ok(path)
}

fn ensure_within_cap(len: usize) -> Result<(), AttachRefusal> {
    let bytes = len as u64;
    if bytes > ATTACHMENT_MAX_BYTES {
        Err(AttachRefusal::TooLarge { bytes })
    } else {
        Ok(())
    }
}

/// Decodes `bytes` as `media_type`, under the image crate's default limits,
/// which bound a decompression bomb. A JPEG's EXIF orientation is applied
/// here: the API does not read the tag, so the pixels must already be upright.
fn decode(bytes: &[u8], media_type: ImageType) -> Result<DynamicImage, AttachRefusal> {
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format_of(media_type))
        .into_decoder()
        .map_err(|_| AttachRefusal::Unreadable)?;
    let orientation = match media_type {
        ImageType::Jpeg => decoder
            .orientation()
            .map_err(|_| AttachRefusal::Unreadable)?,
        _ => Orientation::NoTransforms,
    };
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| AttachRefusal::Unreadable)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// A GIF's size from its header, without decoding its frames.
fn gif_dimensions(bytes: &[u8]) -> Result<(u32, u32), AttachRefusal> {
    let decoder = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Gif)
        .into_decoder()
        .map_err(|_| AttachRefusal::Unreadable)?;
    Ok(decoder.dimensions())
}

/// A JPEG of `image`, flattened onto white because JPEG has no alpha. The
/// encoder writes no EXIF, so the source's metadata does not come along.
fn encode_jpeg(image: &DynamicImage) -> Result<Vec<u8>, AttachRefusal> {
    let white = Rgba([255, 255, 255, 255]);
    let mut canvas = RgbaImage::from_pixel(image.width(), image.height(), white);
    imageops::overlay(&mut canvas, &image.to_rgba8(), 0, 0);
    let opaque = DynamicImage::ImageRgba8(canvas).to_rgb8();
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(opaque)
        .write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY))
        .map_err(|_| AttachRefusal::Unreadable)?;
    Ok(out)
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, AttachRefusal> {
    let mut out = Vec::new();
    image
        .write_with_encoder(PngEncoder::new(&mut out))
        .map_err(|_| AttachRefusal::Unreadable)?;
    Ok(out)
}

fn format_of(media_type: ImageType) -> ImageFormat {
    match media_type {
        ImageType::Png => ImageFormat::Png,
        ImageType::Jpeg => ImageFormat::Jpeg,
        ImageType::Gif => ImageFormat::Gif,
        ImageType::Webp => ImageFormat::WebP,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::codecs::gif::{GifDecoder, GifEncoder};
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::PngEncoder;
    use image::{AnimationDecoder, DynamicImage, Frame, Rgb, RgbImage, Rgba, RgbaImage};

    use super::*;

    const WHITE: [u8; 3] = [255, 255, 255];
    const RED: [u8; 3] = [200, 30, 30];
    const EXIF_HEADER: &[u8] = b"Exif\0\0";

    fn jpeg(image: &DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        image
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 90))
            .unwrap();
        out
    }

    fn png(image: &DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        image.write_with_encoder(PngEncoder::new(&mut out)).unwrap();
        out
    }

    /// A smooth gradient: it compresses badly, like a photo.
    fn photo(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x * 255 / width) as u8, (y * 255 / height) as u8, 128])
        }))
    }

    /// Flat colours and a few rectangles: text and UI, as a screenshot is.
    fn screenshot(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
            if (100..400).contains(&x) && (100..300).contains(&y) {
                Rgb(RED)
            } else if (500..1100).contains(&x) && (50..120).contains(&y) {
                Rgb([30, 90, 200])
            } else {
                Rgb([250, 250, 250])
            }
        }))
    }

    /// An APP1 segment holding one EXIF orientation tag, little-endian.
    fn orientation_app1(orientation: u16) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut payload = EXIF_HEADER.to_vec();
        payload.extend_from_slice(&tiff);
        let length = (payload.len() + 2) as u16;
        let mut segment = vec![0xFF, 0xE1];
        segment.extend_from_slice(&length.to_be_bytes());
        segment.extend_from_slice(&payload);
        segment
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn near(got: [u8; 3], want: [u8; 3]) -> bool {
        got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 8)
    }

    #[test]
    fn big_photo_is_shrunk_to_the_long_side_and_stays_a_jpeg() {
        let input = jpeg(&photo(2400, 1800));
        let out = prepare(&input).unwrap();
        assert_eq!(out.media_type, ImageType::Jpeg);
        assert_eq!(out.width, MAX_SIDE);
        assert!(out.height.abs_diff(1176) <= 1, "height {}", out.height);
        assert_eq!(ImageType::sniff(&out.bytes), Some(ImageType::Jpeg));
        assert!(out.size <= ATTACHMENT_MAX_BYTES);
        assert_eq!(out.size, out.bytes.len() as u64);
        assert_eq!(out.sha256, ContentHash::of(&out.bytes));
    }

    #[test]
    fn flat_png_stays_png_and_is_not_larger() {
        let input = png(&screenshot(1200, 800));
        let out = prepare(&input).unwrap();
        assert_eq!(out.media_type, ImageType::Png);
        assert_eq!((out.width, out.height), (1200, 800));
        assert_eq!(ImageType::sniff(&out.bytes), Some(ImageType::Png));
        assert!(out.bytes.len() <= input.len());
    }

    #[test]
    fn jpeg_orientation_is_applied_and_exif_is_dropped() {
        let plain = jpeg(&photo(200, 100));
        let mut input = plain[..2].to_vec();
        input.extend(orientation_app1(6));
        input.extend_from_slice(&plain[2..]);
        assert!(contains(&input, EXIF_HEADER), "the input carries EXIF");

        let out = prepare(&input).unwrap();
        assert_eq!((out.width, out.height), (100, 200));
        assert!(!contains(&out.bytes, EXIF_HEADER));
    }

    #[test]
    fn webp_alpha_is_flattened_onto_white() {
        let image = RgbaImage::from_fn(64, 64, |x, _| {
            if x < 32 {
                Rgba([0, 0, 0, 0])
            } else {
                Rgba([RED[0], RED[1], RED[2], 255])
            }
        });
        let mut webp = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut webp, ImageFormat::WebP)
            .unwrap();
        let input = webp.into_inner();

        let out = prepare(&input).unwrap();
        assert_eq!(out.media_type, ImageType::Jpeg);
        let decoded = image::load_from_memory(&out.bytes).unwrap().to_rgb8();
        let clear = decoded.get_pixel(10, 10).0;
        assert!(near(clear, WHITE), "transparent pixel came out {clear:?}");
        let solid = decoded.get_pixel(50, 32).0;
        assert!(near(solid, RED), "opaque pixel came out {solid:?}");
    }

    #[test]
    fn animated_gif_passes_through_unchanged() {
        let frames = vec![
            Frame::new(RgbaImage::from_pixel(40, 30, Rgba([255, 0, 0, 255]))),
            Frame::new(RgbaImage::from_pixel(40, 30, Rgba([0, 0, 255, 255]))),
        ];
        let mut input = Vec::new();
        GifEncoder::new(&mut input).encode_frames(frames).unwrap();

        let frame_count = GifDecoder::new(Cursor::new(input.as_slice()))
            .unwrap()
            .into_frames()
            .count();
        assert_eq!(frame_count, 2, "the input is animated");

        let out = prepare(&input).unwrap();
        assert_eq!(out.bytes, input);
        assert_eq!(out.media_type, ImageType::Gif);
        assert_eq!((out.width, out.height), (40, 30));
    }

    #[test]
    fn oversized_gif_is_too_large() {
        let mut input = b"GIF89a".to_vec();
        input.resize(6 + ATTACHMENT_MAX_BYTES as usize, 0);
        let len = input.len() as u64;
        assert_eq!(
            prepare(&input).unwrap_err(),
            AttachRefusal::TooLarge { bytes: len }
        );
    }

    #[test]
    fn noise_png_still_over_the_cap_is_too_large() {
        let mut seed = 1u32;
        let raw: Vec<u8> = (0..MAX_SIDE * MAX_SIDE * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            })
            .collect();
        let noise = RgbaImage::from_raw(MAX_SIDE, MAX_SIDE, raw).unwrap();
        let input = png(&DynamicImage::ImageRgba8(noise));
        match prepare(&input) {
            Err(AttachRefusal::TooLarge { bytes }) => {
                assert!(bytes > ATTACHMENT_MAX_BYTES, "{bytes}");
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn a_pdf_is_not_an_image() {
        let input = b"%PDF-1.4 \n1 0 obj << /Type /Catalog >> endobj\n";
        assert_eq!(prepare(input).unwrap_err(), AttachRefusal::NotAnImage);
    }

    #[test]
    fn a_truncated_png_is_unreadable() {
        let input = png(&photo(16, 16));
        assert!(input.len() > 40);
        assert_eq!(
            prepare(&input[..40]).unwrap_err(),
            AttachRefusal::Unreadable
        );
    }

    #[test]
    fn store_writes_the_bytes_once_under_their_hash() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare(&png(&photo(8, 8))).unwrap();

        let path = store(dir.path(), &prepared).unwrap();
        assert_eq!(
            path,
            dir.path()
                .join("attachments")
                .join(prepared.sha256.to_hex())
        );
        assert_eq!(fs::read(&path).unwrap(), prepared.bytes);

        // A second store leaves the file alone: a rewrite would show here.
        fs::write(&path, b"kept").unwrap();
        assert_eq!(store(dir.path(), &prepared).unwrap(), path);
        assert_eq!(fs::read(&path).unwrap(), b"kept");

        // Only the stored file is there: no temporary file was left behind.
        let entries = fs::read_dir(attachment_dir(dir.path())).unwrap().count();
        assert_eq!(entries, 1);
    }
}
