use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct DependencyPaths {
    pub yt_dlp_binary: PathBuf,
    pub bin_dir: PathBuf,
}

pub async fn ensure_binaries(bin_dir: &Path, auto_download: bool) -> Result<DependencyPaths> {
    let yt_dlp_binary = bin_dir.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
    let ffmpeg_binary = bin_dir.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });

    if yt_dlp_binary.exists() && ffmpeg_binary.exists() {
        tracing::info!(bin_dir = %bin_dir.display(), "using local portable yt-dlp and ffmpeg");
        return Ok(DependencyPaths {
            yt_dlp_binary,
            bin_dir: bin_dir.to_path_buf(),
        });
    }

    if !auto_download {
        tracing::info!("auto_download disabled, falling back to system PATH for yt-dlp and ffmpeg");
        return Ok(DependencyPaths {
            yt_dlp_binary: PathBuf::from("yt-dlp"),
            bin_dir: PathBuf::new(),
        });
    }

    tracing::info!(bin_dir = %bin_dir.display(), "downloading portable yt-dlp and ffmpeg...");
    tokio::fs::create_dir_all(bin_dir).await?;

    use yt_dlp::client::deps::LibraryInstaller;
    let installer = LibraryInstaller::new(bin_dir.to_path_buf());

    installer.install_youtube(None).await.context("failed to download yt-dlp")?;
    installer.install_ffmpeg(None).await.context("failed to download ffmpeg")?;

    tracing::info!("portable dependencies downloaded successfully");

    Ok(DependencyPaths {
        yt_dlp_binary,
        bin_dir: bin_dir.to_path_buf(),
    })
}
