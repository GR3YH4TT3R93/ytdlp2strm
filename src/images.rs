use anyhow::{Context, bail};
use std::path::{Path, PathBuf};

pub const IMAGE_EXTS: &[&str] = &["webp", "jpg", "jpeg", "png", "gif", "avif"];

/// Detect the actual image format from magic bytes.
pub fn sniff_image_ext(bytes: &[u8]) -> &'static str {
    if bytes.len() < 12 {
        return "bin";
    }
    if bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return "jpg";
    }
    if bytes[0] == 0x89 && bytes[1] == 0x50 && bytes[2] == 0x4E && bytes[3] == 0x47 {
        return "png";
    }
    if &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return "webp";
    }
    if &bytes[0..3] == b"GIF" {
        return "gif";
    }
    // AVIF detection (ftypavif at offset 4)
    if &bytes[4..12] == b"ftypavif" {
        return "avif";
    }
    "bin"
}

/// Find an existing image at `base_path` with any known extension.
pub fn find_existing_image(base_path: &Path) -> Option<PathBuf> {
    let base = base_path.to_string_lossy();
    for ext in IMAGE_EXTS {
        let candidate = PathBuf::from(format!("{base}.{ext}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Delete sibling files at `base_path` with extensions other than `keep`.
pub async fn cleanup_image_variants(base_path: &Path, keep: &Path) {
    let base = base_path.to_string_lossy();
    for ext in IMAGE_EXTS {
        let candidate = PathBuf::from(format!("{base}.{ext}"));
        if candidate != keep && tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            let _ = tokio::fs::remove_file(&candidate).await;
        }
    }
}

/// Download an image and save it with the extension matching its actual
/// content type. `base_path` should have NO extension.
/// Returns the full path including the correct extension.
pub async fn download_image(
    client: &reqwest::Client,
    url: &str,
    base_path: &Path,
) -> anyhow::Result<PathBuf> {
    // Fix protocol-relative URLs from Twitch
    let fixed_url = if url.starts_with("//") {
        format!("https:{}", url)
    } else {
        url.to_string()
    };

    let resp = client
        .get(&fixed_url)
        .send()
        .await
        .with_context(|| format!("image fetch failed for {fixed_url}"))?;

    if !resp.status().is_success() {
        bail!("image fetch failed: HTTP {}", resp.status());
    }

    let bytes = resp.bytes().await.context("reading image body")?;
    let ext = sniff_image_ext(&bytes);
    if ext == "bin" {
        bail!(
            "unrecognized image format from {fixed_url} (first bytes: {:02x?})",
            &bytes[..bytes.len().min(16)]
        );
    }

    let full_path = PathBuf::from(format!("{}.{}", base_path.to_string_lossy(), ext));
    if let Some(parent) = full_path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    tokio::fs::write(&full_path, &bytes)
        .await
        .with_context(|| format!("writing image to {}", full_path.display()))?;

    Ok(full_path)
}
