//! Loading images for vision input: from a file path (`/image <path>`) or the OS clipboard (paste).
//! Both produce a base64-encoded [`ImageAttachment`] plus a short human label for the input block.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use forge_types::ImageAttachment;

/// Hard ceiling on an attached image's encoded size. Providers reject very large images; this stops
/// a giant paste from blowing up the request before it's even sent.
const MAX_BYTES: usize = 20 * 1024 * 1024;

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Read an image file and base64-encode it. MIME type is inferred from the extension.
pub fn load_image_file(path: &str) -> Result<(ImageAttachment, String)> {
    let p = std::path::Path::new(path);
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let media_type = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => {
            return Err(anyhow!(
                "unsupported image type '.{ext}' (png/jpg/gif/webp)"
            ))
        }
    };
    let bytes = std::fs::read(p).with_context(|| format!("reading {path}"))?;
    if bytes.is_empty() {
        return Err(anyhow!("{path} is empty"));
    }
    if bytes.len() > MAX_BYTES {
        return Err(anyhow!(
            "{path} too large ({} MB; max 20 MB)",
            bytes.len() / 1_048_576
        ));
    }
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("image");
    let label = format!("{name} {}KB", bytes.len() / 1024);
    Ok((
        ImageAttachment {
            media_type: media_type.to_string(),
            data_base64: encode(&bytes),
        },
        label,
    ))
}

/// Read an image from the OS clipboard (the paste path). Returns `None` when the clipboard holds no
/// image (it's text, empty, or no clipboard is available). The raw RGBA pixels are PNG-encoded for
/// broad provider support.
pub fn clipboard_image() -> Option<(ImageAttachment, String)> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let img = clipboard.get_image().ok()?;
    let (w, h) = (img.width as u32, img.height as u32);
    let rgba = image::RgbaImage::from_raw(w, h, img.bytes.into_owned())?;
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut png, image::ImageFormat::Png)
        .ok()?;
    let bytes = png.into_inner();
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }
    Some((
        ImageAttachment {
            media_type: "image/png".to_string(),
            data_base64: encode(&bytes),
        },
        format!("PNG {w}x{h}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_extension() {
        assert!(load_image_file("/tmp/whatever.txt").is_err());
    }

    #[test]
    fn loads_a_real_png_and_infers_mime() {
        // A 1x1 PNG written to a temp file round-trips into a base64 attachment.
        let dir = std::env::temp_dir();
        let path = dir.join("forge_test_pixel.png");
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(1, 1))
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&path, buf.into_inner()).unwrap();
        let (att, label) = load_image_file(path.to_str().unwrap()).unwrap();
        assert_eq!(att.media_type, "image/png");
        assert!(!att.data_base64.is_empty());
        assert!(label.contains("forge_test_pixel.png"));
        let _ = std::fs::remove_file(&path);
    }
}
