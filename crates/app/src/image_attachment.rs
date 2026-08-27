//! Bounded attachment image inspection and format-preserving resize.

use std::io::Cursor;

use bytes::Bytes;
use image::{GenericImageView as _, ImageFormat, ImageReader, imageops::FilterType};
use thiserror::Error;

/// Maximum attachment dimension.
pub const MAX_IMAGE_DIMENSION: u32 = 2_000;
/// Hard decoded-pixel admission bound.
pub const MAX_DECODED_PIXELS: u64 = 100_000_000;

/// Image attachment transformation failure.
#[derive(Debug, Error)]
pub enum ImageAttachmentError {
	/// The input is not a supported image.
	#[error("unsupported attachment image format")]
	Unsupported,
	/// Dimensions exceed the bounded decoder admission limit.
	#[error("attachment image dimensions exceed the decode limit")]
	Dimensions,
	/// Decoding or encoding failed.
	#[error("attachment image codec failed")]
	Codec(#[from] image::ImageError),
}

/// A resized or unchanged image preserving its detected format and MIME type.
#[derive(Clone, Debug)]
pub struct PreparedImage {
	/// MIME type matching `format`.
	pub media_type: &'static str,
	/// Detected image format.
	pub format:     ImageFormat,
	/// Original dimensions.
	pub original:   (u32, u32),
	/// Emitted dimensions.
	pub dimensions: (u32, u32),
	/// Encoded attachment bytes.
	pub bytes:      Bytes,
}

/// Inspects and optionally resizes a supported image. Small images remain
/// byte-for-byte identical; oversized images are downscaled without changing
/// their format.
pub fn prepare(bytes: Bytes, auto_resize: bool) -> Result<PreparedImage, ImageAttachmentError> {
	let format = image::guess_format(&bytes).map_err(|_| ImageAttachmentError::Unsupported)?;
	let media_type = media_type(format).ok_or(ImageAttachmentError::Unsupported)?;
	let reader = ImageReader::with_format(Cursor::new(bytes.as_ref()), format);
	let dimensions = reader.into_dimensions()?;
	if u64::from(dimensions.0).saturating_mul(u64::from(dimensions.1)) > MAX_DECODED_PIXELS {
		return Err(ImageAttachmentError::Dimensions);
	}
	if !auto_resize || (dimensions.0 <= MAX_IMAGE_DIMENSION && dimensions.1 <= MAX_IMAGE_DIMENSION) {
		return Ok(PreparedImage { media_type, format, original: dimensions, dimensions, bytes });
	}
	let image = image::load_from_memory_with_format(&bytes, format)?;
	let resized = image.resize(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION, FilterType::Lanczos3);
	let output_dimensions = resized.dimensions();
	let mut output = Cursor::new(Vec::new());
	resized.write_to(&mut output, format)?;
	Ok(PreparedImage {
		media_type,
		format,
		original: dimensions,
		dimensions: output_dimensions,
		bytes: Bytes::from(output.into_inner()),
	})
}

fn media_type(format: ImageFormat) -> Option<&'static str> {
	match format {
		ImageFormat::Png => Some("image/png"),
		ImageFormat::Jpeg => Some("image/jpeg"),
		ImageFormat::Gif => Some("image/gif"),
		ImageFormat::WebP => Some("image/webp"),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use image::DynamicImage;

	use super::*;

	#[test]
	fn small_png_is_not_reencoded() {
		let image = DynamicImage::new_rgba8(4, 3);
		let mut output = Cursor::new(Vec::new());
		image.write_to(&mut output, ImageFormat::Png).expect("png");
		let bytes = Bytes::from(output.into_inner());
		let prepared = prepare(bytes.clone(), true).expect("prepare");
		assert_eq!(prepared.bytes, bytes);
		assert_eq!(prepared.dimensions, (4, 3));
	}
}
