use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use which::which;

pub struct DependencyPaths {
    pub yt_dlp_binary: PathBuf,
    pub bin_dir: PathBuf,
}

pub async fn ensure_binaries(bin_dir: &Path, auto_download: bool) -> Result<DependencyPaths> {
    let yt_dlp_name = if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    };
    let ffmpeg_name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };

    let local_ytdlp = bin_dir.join(yt_dlp_name);
    let local_ffmpeg = bin_dir.join(ffmpeg_name);

    // 1. Check local portable ./bin directory
    if local_ytdlp.exists() && local_ffmpeg.exists() {
        tracing::info!(bin_dir = %bin_dir.display(), "using local portable yt-dlp and ffmpeg");
        return Ok(DependencyPaths {
            yt_dlp_binary: local_ytdlp,
            bin_dir: bin_dir.to_path_buf(),
        });
    }

    // 2. Check system PATH (equivalent to `command -v`)
    let sys_ytdlp = which(yt_dlp_name).ok();
    let sys_ffmpeg = which(ffmpeg_name).ok();

    if let (Some(yt), Some(_ff)) = (&sys_ytdlp, &sys_ffmpeg) {
        tracing::info!(
            yt_dlp = %yt.display(),
            "found yt-dlp and ffmpeg in system PATH, using system binaries"
        );
        return Ok(DependencyPaths {
            yt_dlp_binary: PathBuf::from(yt_dlp_name), // Use the name so it resolves via PATH
            bin_dir: PathBuf::new(),                   // Empty means don't inject anything
        });
    }

    // 3. If missing from both, and auto_download is disabled, fail or fallback
    if !auto_download {
        tracing::warn!(
            "auto_download is disabled, but yt-dlp/ffmpeg were not found in PATH or ./bin. Will attempt to use system 'yt-dlp' and hope for the best."
        );
        return Ok(DependencyPaths {
            yt_dlp_binary: PathBuf::from(yt_dlp_name),
            bin_dir: PathBuf::new(),
        });
    }

    // 4. Download them to ./bin
    tracing::info!(bin_dir = %bin_dir.display(), "yt-dlp and ffmpeg missing from PATH, downloading portable binaries...");
    tokio::fs::create_dir_all(bin_dir).await?;

    use yt_dlp::client::deps::LibraryInstaller;
    let installer = LibraryInstaller::new(bin_dir.to_path_buf());

    installer
        .install_youtube(None)
        .await
        .context("failed to download yt-dlp")?;
    installer
        .install_ffmpeg(None)
        .await
        .context("failed to download ffmpeg")?;

    tracing::info!("portable dependencies downloaded successfully");

    Ok(DependencyPaths {
        yt_dlp_binary: local_ytdlp,
        bin_dir: bin_dir.to_path_buf(),
    })
}
