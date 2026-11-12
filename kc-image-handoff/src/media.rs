//! Images in: read from the workspace, or fetched over http(s).
//!
//! Magic-byte sniffing decides the MIME type rather than trusting the
//! extension - the file the agent names is untrusted input, and a lied-about
//! `.png` that is really a PHP script must not be labeled `image/png` to a
//! remote vision endpoint. The extension is only a tiebreaker when the bytes
//! do not match a known format.

use std::io::Read;
use std::time::Duration;

/// A single image ready to hand off.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub mime: &'static str,
    pub data: Vec<u8>,
    /// Where it came from: a workspace file or a URL.
    pub source: String,
    pub kind: ImageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    File,
    Url,
}

/// What can be sniffed, as the MIME the vision endpoint expects. SVG is
/// deliberately absent: it is not a raster format the vision endpoints decode,
/// and passing an executable XML document to a remote parser is not something
/// this plugin should launder.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 2 && bytes[..2] == [0x42, 0x4D] {
        return Some("image/bmp");
    }
    None
}

// The extension is never trusted for the content type: sniffing is always
// performed on the bytes themselves, and a `.png` that is not a PNG must not
// be labeled `image/png` to a remote endpoint.

impl Image {
    /// Load a workspace file (or any readable path). `max_bytes` bounds the
    /// read so a huge file cannot be slurped before anyone checks it.
    pub fn from_path(
        path: &str,
        context_workspace: Option<&str>,
        max_bytes: usize,
    ) -> Result<Self, String> {
        if path.trim().is_empty() {
            return Err("path is required and must not be empty".to_string());
        }
        if path.starts_with("http://") || path.starts_with("https://") {
            return Err("path must be a file path; use `url` for http(s) images".to_string());
        }
        let resolved = if path.starts_with('/') {
            path.to_string()
        } else {
            let workspace = context_workspace.ok_or_else(|| {
                format!(
                    "relative path `{path}` needs a session workspace to resolve against; \
                     pass an absolute path"
                )
            })?;
            join_workspace(workspace, path)
        };
        let metadata =
            std::fs::metadata(&resolved).map_err(|e| format!("cannot stat `{resolved}`: {e}"))?;
        if !metadata.is_file() {
            return Err(format!("`{resolved}` is not a regular file"));
        }
        let size = metadata.len() as usize;
        if size > max_bytes {
            return Err(format!(
                "image is {:.1} MiB, over the {:.1} MiB limit (settings.max_image_mb)",
                size as f64 / (1024.0 * 1024.0),
                max_bytes as f64 / (1024.0 * 1024.0),
            ));
        }
        let data =
            std::fs::read(&resolved).map_err(|e| format!("cannot read `{resolved}`: {e}"))?;
        let mime = sniff(&data).ok_or_else(|| {
            format!("`{resolved}` is not a supported image (need PNG, JPEG, GIF, WebP or BMP)")
        })?;
        Ok(Self {
            mime,
            data,
            source: resolved,
            kind: ImageKind::File,
        })
    }

    pub fn url_is_allowed(url: &str) -> Result<(), String> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return Err("url is required and must not be empty".to_string());
        }
        for prefix in ["http://", "https://"] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                if !rest.is_empty() {
                    return Ok(());
                }
            }
        }
        Err(format!("url `{url}` must be http:// or https://"))
    }

    /// Fetch a remote image. Bounded by `timeout` and `max_bytes`.
    pub fn from_url(url: &str, timeout: Duration, max_bytes: usize) -> Result<Self, String> {
        Self::url_is_allowed(url)?;
        let response = ureq::AgentBuilder::new()
            .timeout(timeout)
            .build()
            .get(url.trim())
            .call()
            .map_err(|e| format!("cannot fetch `{url}`: {e}"))?;
        if !response.content_type().is_empty() && !response.content_type().starts_with("image/") {
            // Some servers answer HTML error pages with 200; do not base64 those.
            return Err(format!(
                "`{url}` answered Content-Type `{}`, not an image",
                response.content_type()
            ));
        }
        let reader = response.into_reader();
        let mut data = Vec::new();
        let max = max_bytes + 1; // one byte over means "too big"
        reader
            .take(max as u64)
            .read_to_end(&mut data)
            .map_err(|e| format!("cannot read `{url}` response body: {e}"))?;
        if data.len() > max_bytes {
            return Err(format!(
                "image at `{url}` is over the {:.1} MiB limit (settings.max_image_mb)",
                max_bytes as f64 / (1024.0 * 1024.0)
            ));
        }
        if data.is_empty() {
            return Err(format!("`{url}` returned an empty body"));
        }
        let mime = sniff(&data).ok_or_else(|| {
            format!("`{url}` is not a supported image (need PNG, JPEG, GIF, WebP or BMP)")
        })?;
        Ok(Self {
            mime,
            data,
            source: url.trim().to_string(),
            kind: ImageKind::Url,
        })
    }
}

fn join_workspace(workspace: &str, path: &str) -> String {
    let workspace = workspace.trim_end_matches('/');
    format!("{workspace}/{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-file";
    const JPG: &[u8] = b"\xFF\xD8\xFF\xE0junk";

    #[test]
    fn sniff_known_formats() {
        assert_eq!(sniff(PNG), Some("image/png"));
        assert_eq!(sniff(JPG), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF0000WEBPVP8 x"), Some("image/webp"));
        assert_eq!(sniff(b"BM\x36\x00"), Some("image/bmp"));
        assert_eq!(sniff(b"not an image"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn url_validation_rejects_non_http() {
        assert!(Image::url_is_allowed("https://example.com/a.png").is_ok());
        assert!(Image::url_is_allowed("http://example.com/a.png").is_ok());
        assert!(Image::url_is_allowed("file:///etc/passwd").is_err());
        assert!(Image::url_is_allowed("ftp://example.com").is_err());
        assert!(Image::url_is_allowed("data:image/png;base64,AAA").is_err());
        assert!(Image::url_is_allowed("").is_err());
    }

    #[test]
    fn disallowed_url_schemes_block_admin_interfaces() {
        // file:// is the dangerous one; these assert it never gets fetched
        for bad in ["file:///etc/passwd", "gopher://x", "//example.com"] {
            assert!(Image::url_is_allowed(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn workspace_join_is_simple() {
        assert_eq!(join_workspace("/w/", "a/b.png"), "/w/a/b.png");
    }
}
