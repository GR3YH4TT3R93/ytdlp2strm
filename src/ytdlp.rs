use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct Ytdlp {
    binary: PathBuf,
    extra_args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ChannelEntry {
    pub id: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub duration_secs: Option<u64>,
    pub upload_date: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub name: String,
    pub channel_id: Option<String>,
    pub thumbnail_url: Option<String>,
    pub description: Option<String>,
    pub entries: Vec<ChannelEntry>,
}

impl Ytdlp {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            extra_args: Vec::new(),
        }
    }

    /// Build with user-supplied extra args (from `[youtube] extra_args`).
    pub fn from_path_with_args(extra_args: Vec<String>) -> Self {
        let mut ytdlp = Self::new("yt-dlp");
        ytdlp.extra_args = extra_args;
        ytdlp
    }

    #[allow(dead_code)]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Resolve a URL to yt-dlp's full JSON metadata (`yt-dlp -J`).
    pub async fn resolve(&self, url: &str) -> Result<Value> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("-J")
            .arg("--no-warnings")
            .arg("--no-playlist")
            .args(&self.extra_args)
            .arg("--")
            .arg(url);

        let output = tokio::time::timeout(RESOLVE_TIMEOUT, cmd.output())
            .await
            .context("yt-dlp resolve timed out")?
            .context("failed to spawn yt-dlp — is it installed and on $PATH?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("yt-dlp exited with {}: {}", output.status, stderr.trim());
        }

        let json: Value =
            serde_json::from_slice(&output.stdout).context("yt-dlp produced invalid JSON")?;
        Ok(json)
    }

    /// Fetch the format-specific media playlist URL for a given format ID.
    pub async fn stream_url(&self, format_id: &str, video_url: &str) -> Result<String> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("-g")
            .arg("-f")
            .arg(format_id)
            .arg("--no-warnings")
            .arg("--no-playlist")
            .args(&self.extra_args)
            .arg("--")
            .arg(video_url);

        let output = tokio::time::timeout(RESOLVE_TIMEOUT, cmd.output())
            .await
            .context("yt-dlp stream_url timed out")?
            .context("failed to spawn yt-dlp")?;

        if !output.status.success() {
            bail!(
                "yt-dlp -g -f {format_id} failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let url = String::from_utf8(output.stdout)
            .context("yt-dlp output not UTF-8")?
            .lines()
            .next()
            .ok_or_else(|| anyhow::anyhow!("yt-dlp -g -f {format_id} produced no output"))?
            .trim()
            .to_string();

        if url.is_empty() {
            bail!("yt-dlp -g -f {format_id} returned an empty URL");
        }

        Ok(url)
    }

    /// Enumerate a channel or playlist using `--flat-playlist`.
    pub async fn list_channel(&self, channel_url: &str, limit: usize) -> Result<ChannelInfo> {
        let normalized = normalize_playlist_url(channel_url);

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-J")
            .arg("--flat-playlist")
            .arg("--playlist-end")
            .arg(limit.to_string())
            .arg("--no-warnings")
            .args(&self.extra_args)
            .arg("--")
            .arg(&normalized);

        let output = tokio::time::timeout(LIST_TIMEOUT, cmd.output())
            .await
            .context("yt-dlp channel listing timed out")?
            .context("failed to spawn yt-dlp")?;

        if !output.status.success() {
            bail!(
                "yt-dlp listing failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let json: Value =
            serde_json::from_slice(&output.stdout).context("yt-dlp produced invalid JSON")?;

        let typ = json.get("_type").and_then(|v| v.as_str()).unwrap_or("");
        if typ == "video" || json.get("entries").is_none() {
            bail!("URL is not a channel or playlist (got _type={typ:?})");
        }

        let name = json
            .get("channel")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("uploader").and_then(|v| v.as_str()))
            .or_else(|| json.get("title").and_then(|v| v.as_str()))
            .unwrap_or("Unknown")
            .to_string();

        let channel_id = json
            .get("channel_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let thumbnail_url = json
            .get("thumbnails")
            .and_then(|v| v.as_array())
            .and_then(|arr| best_thumbnail(arr));

        let description = json
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let entries: Vec<ChannelEntry> = json
            .get("entries")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let id = e.get("id").and_then(|v| v.as_str())?.to_string();
                        if id.is_empty() {
                            return None;
                        }
                        let title = e
                            .get("title")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .unwrap_or(&id)
                            .to_string();
                        let thumbnail_url = e
                            .get("thumbnails")
                            .and_then(|v| v.as_array())
                            .and_then(|arr| best_thumbnail(arr));
                        let duration_secs = e.get("duration").and_then(|v| v.as_u64());
                        let upload_date = e
                            .get("upload_date")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let description = e
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        Some(ChannelEntry {
                            id,
                            title,
                            thumbnail_url,
                            duration_secs,
                            upload_date,
                            description,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(ChannelInfo {
            name,
            channel_id,
            thumbnail_url,
            description,
            entries,
        })
    }
}

/// Pick the highest-resolution thumbnail from a yt-dlp `thumbnails` array.
/// Pick the highest-resolution thumbnail from a yt-dlp `thumbnails` array.
/// Format is not considered — WebP, JPEG, and PNG are all supported by
/// Jellyfin/Kodi/Emby/Plex. The download step sniffs actual bytes and
/// names the file accordingly.
fn best_thumbnail(thumbnails: &[Value]) -> Option<String> {
    let mut best: Option<(&Value, (u64, i64))> = None;
    for t in thumbnails {
        let Some(url) = t.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let width = t.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
        let height = t.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
        let pref = t.get("preference").and_then(|v| v.as_i64()).unwrap_or(0);
        let score = (width * height, pref);
        if best.map_or(true, |(_, b)| score > b) {
            best = Some((t, score));
        }
    }
    best.and_then(|(t, _)| t.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()))
}

fn normalize_playlist_url(url: &str) -> String {
    if url.contains("/playlist?") || url.contains("/playlist/") {
        return url.to_string();
    }
    if !url.contains("youtube.com") && !url.contains("youtu.be") {
        return url.to_string();
    }
    let tab_suffixes = [
        "/videos",
        "/streams",
        "/shorts",
        "/playlists",
        "/community",
        "/featured",
        "/podcasts",
        "/releases",
    ];
    let lower = url.to_lowercase();
    if tab_suffixes.iter().any(|s| lower.contains(s)) {
        return url.to_string();
    }
    if url.contains("/watch?") {
        return url.to_string();
    }
    let base = url.trim_end_matches('/');
    let base = base.split('?').next().unwrap_or(base);
    format!("{base}/videos")
}

pub struct DownloadOptions<'a> {
    /// Output template, e.g. `<dir>/%(id)s.%(ext)s`.
    pub output_template: &'a std::path::Path,
    /// SponsorBlock categories to physically remove. Empty = no removal.
    pub sponsorblock_categories: &'a [String],
    pub sponsorblock_api: Option<&'a str>,
}

impl Ytdlp {
    // ... existing methods (new, from_path_with_args, resolve, stream_url, list_channel) ...

    /// Download a video with yt-dlp, applying SponsorBlock removal if
    /// categories are provided. Requires ffmpeg on PATH for the merge and
    /// for the SB re-mux. Returns the final file path.
    pub async fn download_video(
        &self,
        video_id: &str,
        opts: DownloadOptions<'_>,
    ) -> Result<PathBuf> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-f")
            .arg("bv*[ext=mp4]+ba[ext=m4a]/bv*[vcodec^=avc1]+ba/bv*+ba/b")
            .arg("--merge-output-format")
            .arg("mp4")
            .arg("--no-warnings")
            .arg("--no-playlist")
            .arg("--no-progress")
            .arg("--newline")
            .arg("--print")
            .arg("after_move:filepath")
            .arg("-o")
            .arg(opts.output_template);

        if !opts.sponsorblock_categories.is_empty() {
            cmd.arg("--sponsorblock-remove")
                .arg(opts.sponsorblock_categories.join(","));
            if let Some(api) = opts.sponsorblock_api {
                cmd.arg("--sponsorblock-api").arg(api);
            }
        }

        cmd.args(&self.extra_args);
        cmd.arg("--").arg(&url);

        // Downloads can take several minutes for long videos.
        let output = tokio::time::timeout(Duration::from_secs(3600), cmd.output())
            .await
            .context("yt-dlp download timed out (1h)")?
            .context("failed to spawn yt-dlp")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "yt-dlp download failed (exit {}): {}",
                output.status,
                stderr.trim()
            );
        }

        let path = String::from_utf8_lossy(&output.stdout)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty() && l.trim().contains(std::path::MAIN_SEPARATOR))
            .map(|s| PathBuf::from(s.trim()))
            .ok_or_else(|| anyhow::anyhow!("yt-dlp did not print a final output path"))?;

        if !path.exists() {
            bail!(
                "yt-dlp reported output at {} but no file exists there",
                path.display()
            );
        }

        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_playlist_url;

    #[test]
    fn bare_handle_gets_videos() {
        assert_eq!(
            normalize_playlist_url("https://www.youtube.com/@mkbhd"),
            "https://www.youtube.com/@mkbhd/videos"
        );
    }

    #[test]
    fn trailing_slash_stripped() {
        assert_eq!(
            normalize_playlist_url("https://www.youtube.com/@mkbhd/"),
            "https://www.youtube.com/@mkbhd/videos"
        );
    }

    #[test]
    fn existing_tab_preserved() {
        assert_eq!(
            normalize_playlist_url("https://www.youtube.com/@mkbhd/streams"),
            "https://www.youtube.com/@mkbhd/streams"
        );
    }

    #[test]
    fn playlist_preserved() {
        let url = "https://www.youtube.com/playlist?list=PLabc123";
        assert_eq!(normalize_playlist_url(url), url);
    }

    #[test]
    fn channel_id_preserved() {
        assert_eq!(
            normalize_playlist_url("https://www.youtube.com/channel/UCBJycsmduvYEL83R_U4JriQ"),
            "https://www.youtube.com/channel/UCBJycsmduvYEL83R_U4JriQ/videos"
        );
    }

    #[test]
    fn query_string_stripped_on_bare() {
        assert_eq!(
            normalize_playlist_url("https://www.youtube.com/@mkbhd?si=xyz"),
            "https://www.youtube.com/@mkbhd/videos"
        );
    }
}
