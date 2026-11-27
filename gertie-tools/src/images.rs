//! Images a tool hands the model (#50): a picture `read_file` opened,
//! a screenshot. Every one passes through [`fit`] first, so what goes
//! on the wire is something every endpoint takes: the long edge at
//! most [`LONG_EDGE`] pixels (the size the endpoints scale down to
//! anyway, so nothing the model could see is lost), and the encoded
//! bytes under [`MAX_ENCODED`] (Anthropic refuses an image over 5 MB,
//! base64 grows bytes by a third).

use base64::Engine as _;

/// The largest image file a tool will open, the same cap the composer
/// puts on an attached image.
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// The long edge an image is scaled to, at most.
const LONG_EDGE: u32 = 1568;

/// The encoded size an image must fit under, before base64.
const MAX_ENCODED: usize = 3_500_000;

/// An image ready for the wire, and what the model is told about it.
pub struct Fitted {
    pub data_url: String,
    /// The size the model sees.
    pub width: u32,
    pub height: u32,
    /// The size the file has.
    pub original: (u32, u32),
}

impl Fitted {
    /// "1920x1200, shown at 1568x980", or "800x600" when unscaled.
    pub fn describe(&self) -> String {
        let (width, height) = self.original;
        if (width, height) == (self.width, self.height) {
            format!("{width}x{height}")
        } else {
            format!("{width}x{height}, shown at {}x{}", self.width, self.height)
        }
    }
}

/// Decodes `bytes` and makes them wire-ready: an image already within
/// the limits passes through as it is (no re-encode, no quality lost),
/// a larger one is scaled down and re-encoded, PNG first and JPEG when
/// the PNG is still too heavy (a photo).
pub fn fit(bytes: &[u8]) -> Result<Fitted, String> {
    let format = image::guess_format(bytes)
        .map_err(|error| format!("not an image gertie reads ({error})"))?;
    let media_type = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        other => return Err(format!("{other:?} images aren't readable by models")),
    };
    let decoded = image::load_from_memory_with_format(bytes, format)
        .map_err(|error| format!("the image doesn't decode ({error})"))?;
    let original = (decoded.width(), decoded.height());
    if original.0.max(original.1) <= LONG_EDGE && bytes.len() <= MAX_ENCODED {
        return Ok(Fitted {
            data_url: data_url(media_type, bytes),
            width: original.0,
            height: original.1,
            original,
        });
    }
    let scaled = if original.0.max(original.1) > LONG_EDGE {
        decoded.resize(
            LONG_EDGE,
            LONG_EDGE,
            image::imageops::FilterType::CatmullRom,
        )
    } else {
        decoded
    };
    let mut png = Vec::new();
    scaled
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| format!("couldn't re-encode the image ({error})"))?;
    let (media_type, encoded) = if png.len() <= MAX_ENCODED {
        ("image/png", png)
    } else {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85)
            .encode_image(&scaled.to_rgb8())
            .map_err(|error| format!("couldn't re-encode the image ({error})"))?;
        ("image/jpeg", jpeg)
    };
    Ok(Fitted {
        data_url: data_url(media_type, &encoded),
        width: scaled.width(),
        height: scaled.height(),
        original,
    })
}

fn data_url(media_type: &str, bytes: &[u8]) -> String {
    format!(
        "data:{media_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// Whether a path names an image by its extension: what `read_file`
/// opens as a picture instead of as text.
pub fn is_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn a_small_image_passes_through_untouched() {
        let bytes = png(40, 30);
        let fitted = fit(&bytes).unwrap();
        assert_eq!((fitted.width, fitted.height), (40, 30));
        assert_eq!(fitted.describe(), "40x30");
        let expected = data_url("image/png", &bytes);
        assert_eq!(
            fitted.data_url, expected,
            "no re-encode when it already fits"
        );
    }

    #[test]
    fn a_large_image_scales_to_the_long_edge() {
        let fitted = fit(&png(3136, 1000)).unwrap();
        assert_eq!(fitted.width, LONG_EDGE);
        assert_eq!(fitted.height, 500);
        assert_eq!(fitted.describe(), "3136x1000, shown at 1568x500");
        assert!(fitted.data_url.starts_with("data:image/"));
    }

    #[test]
    fn text_is_not_an_image() {
        let refused = fit(b"fn main() {}").err().unwrap();
        assert!(refused.contains("not an image"), "{refused}");
    }

    #[test]
    fn image_paths_go_by_extension() {
        assert!(is_image_path(std::path::Path::new("shots/a.PNG")));
        assert!(is_image_path(std::path::Path::new("b.webp")));
        assert!(!is_image_path(std::path::Path::new("c.svg")));
        assert!(!is_image_path(std::path::Path::new("Makefile")));
    }
}
