//! A composited frame of pixels and its encodings.
//!
//! [`Frame`]s are produced on the compositor thread by
//! [`crate::CompositorHandle`] capture commands and travelled back as plain
//! data: `pixels` is tightly packed, row-major, non-premultiplied RGBA8 —
//! the exact layout `image::RgbaImage` and PNG/WebP encoders consume.

use image::ImageEncoder;

use crate::error::Result;

/// A captured frame of composited pixels.
///
/// Per-window captures have a transparent background outside the window's
/// surfaces; full-output captures are composited over opaque black.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Non-premultiplied RGBA8 pixels, row-major, `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

impl Frame {
    /// The RGBA8 pixel at `(x, y)`; panics when out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * self.width + x) * 4) as usize;
        self.pixels[offset..offset + 4].try_into().unwrap()
    }

    /// Encodes the frame as a PNG image (lossless).
    pub fn encode_png(&self) -> Result<Vec<u8>> {
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded).write_image(
            &self.pixels,
            self.width,
            self.height,
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(encoded)
    }

    /// Encodes the frame as a lossless WebP image.
    pub fn encode_webp(&self) -> Result<Vec<u8>> {
        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded).write_image(
            &self.pixels,
            self.width,
            self.height,
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(encoded)
    }

    /// Converts a tiny-skia pixmap (premultiplied RGBA8) into a [`Frame`]
    /// (straight-alpha RGBA8).
    pub(crate) fn from_pixmap(pixmap: tiny_skia::Pixmap) -> Self {
        Self {
            width: pixmap.width(),
            height: pixmap.height(),
            pixels: pixmap.take_demultiplied(),
        }
    }
}
