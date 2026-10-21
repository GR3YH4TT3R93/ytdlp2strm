use anyhow::{Context, bail};
use image::ImageFormat;
use std::path::{Path, PathBuf};

pub const IMAGE_EXTS: &[&str] = &["webp", "jpg", "jpeg", "png", "gif", "avif"];

/// Detect the actual image format from magic bytes (used for logging).
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

/// Download an image, convert it to WebP, and save it.
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

    // Ask the server to send WebP if it supports content negotiation
    let resp = client
        .get(&fixed_url)
        .header(
            reqwest::header::ACCEPT,
            "image/webp,image/jpeg,image/png,image/*",
        )
        .send()
        .await
        .with_context(|| format!("image fetch failed for {fixed_url}"))?;

    if !resp.status().is_success() {
        bail!("image fetch failed: HTTP {}", resp.status());
    }

    let bytes = resp.bytes().await.context("reading image body")?;

    // Decode the image (supports AVIF/WebP/PNG/JPG)
    let img = image::load_from_memory(&bytes).with_context(|| {
        format!(
            "failed to decode image from {fixed_url} (format sniff: {})",
            sniff_image_ext(&bytes)
        )
    })?;

    // ALWAYS save as .webp
    let full_path = PathBuf::from(format!("{}.webp", base_path.to_string_lossy()));

    if let Some(parent) = full_path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    // Encode to WebP in a background thread
    let path_clone = full_path.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // Preserve transparency if the image has it (WebP supports alpha)
        img.save_with_format(&path_clone, ImageFormat::WebP)?;
        Ok(())
    })
    .await
    .context("image conversion task panicked")??;

    Ok(full_path)
}
