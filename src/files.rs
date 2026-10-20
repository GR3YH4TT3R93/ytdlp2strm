use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::fs;

use crate::images::IMAGE_EXTS;

#[derive(Debug)]
pub enum StrmOutcome {
    Written(PathBuf),
    Skipped(PathBuf),
}

pub async fn write_strm_file(
    dir: &Path,
    _video_id: &str,
    title: &str,
    play_url: &str,
    force: bool,
) -> std::io::Result<StrmOutcome> {
    let safe_title = sanitize_filename(title);
    let filename = format!("{safe_title}.strm");
    let path = dir.join(filename);

    if !force && fs::metadata(&path).await.is_ok() {
        return Ok(StrmOutcome::Skipped(path));
    }

    fs::create_dir_all(dir).await?;
    fs::write(&path, format!("{play_url}\n")).await?;
    Ok(StrmOutcome::Written(path))
}

#[derive(Default)]
pub struct ExistingFiles {
    pub strms: HashMap<String, PathBuf>,
    pub nfos: HashMap<String, PathBuf>,
    pub thumbs: HashMap<String, PathBuf>,
}

pub async fn scan_existing_files(dir: &Path) -> ExistingFiles {
    let mut result = ExistingFiles::default();
    let mut rd = match fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return result,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(".strm") {
            if let Ok(content) = fs::read_to_string(&path).await {
                let content = content.trim();
                if let Some(id) = content.split('/').last() {
                    if !id.is_empty() {
                        let id = id.to_string();
                        result.strms.insert(id.clone(), path);

                        let nfo_path = dir.join(format!("{stem}.nfo"));
                        if nfo_path.exists() {
                            result.nfos.insert(id.clone(), nfo_path);
                        }

                        for ext in IMAGE_EXTS {
                            let thumb_path = dir.join(format!("{stem}-thumb.{ext}"));
                            if thumb_path.exists() {
                                result.thumbs.insert(id.clone(), thumb_path);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    result
}

pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    let trimmed = cleaned.trim().trim_matches('.').trim();
    let capped: String = trimmed.chars().take(200).collect();
    if capped.is_empty() {
        "untitled".to_string()
    } else {
        capped
    }
}

pub async fn read_cache_file(cache_dir: &Path, filename: &str, ttl: Duration) -> Option<String> {
    let path = cache_dir.join(filename);
    let meta = fs::metadata(&path).await.ok()?;
    let modified = meta.modified().ok()?;

    if let Ok(age) = SystemTime::now().duration_since(modified) {
        if age > ttl {
            return None;
        }
    } else {
        return None;
    }

    fs::read_to_string(&path).await.ok()
}

pub async fn write_cache_file(cache_dir: &Path, filename: &str, content: &str) {
    let path = cache_dir.join(filename);
    if let Err(e) = fs::write(&path, content).await {
        tracing::warn!(path = %path.display(), error = %e, "failed to write cache file");
    }
}

pub async fn move_directory(from: &Path, to: &Path) -> anyhow::Result<()> {
    if from == to || !from.exists() {
        return Ok(());
    }

    if fs::rename(from, to).await.is_ok() {
        tracing::info!(from = %from.display(), to = %to.display(), "moved directory");
        return Ok(());
    }

    tracing::info!(from = %from.display(), to = %to.display(), "cross-device move detected, copying in background...");

    let from = from.to_path_buf();
    let to = to.to_path_buf();

    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&to)?;
        copy_dir_all_blocking(&from, &to)?;
        let _ = std::fs::remove_dir_all(&from);
        Ok::<(), std::io::Error>(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("Task join error: {e}"))??;

    tracing::info!("directory move complete");
    Ok(())
}

fn copy_dir_all_blocking(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all_blocking(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}
