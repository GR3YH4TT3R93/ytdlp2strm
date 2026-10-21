use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(3600);

/// Rate-limit mitigation. Applied to every yt-dlp invocation that runs as part
/// of a bulk operation (channel enumeration, per-video metadata fetch, and
/// download) but deliberately NOT to `stream_url`, which is called on-demand
/// for playback and must return fast.
fn add_rate_limit_args(cmd: &mut Command) {
    cmd.arg("--sleep-requests")
        .arg("1")
        .arg("--sleep-interval")
        .arg("2")
        .arg("--retries")
        .arg("5")
        .arg("--retry-sleep")
        .arg("10");
}

#[derive(Debug, Clone)]
pub struct Ytdlp {
    binary: PathBuf,
    bin_dir: PathBuf,
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
    pub banner_url: Option<String>,
    pub description: Option<String>,
    pub entries: Vec<ChannelEntry>,
}

pub struct DownloadOptions<'a> {
    pub output_template: &'a Path,
    pub sponsorblock_categories: &'a [String],
    pub sponsorblock_api: Option<&'a str>,
}

impl Ytdlp {
    pub fn from_path_with_args(binary: PathBuf, bin_dir: PathBuf, extra_args: Vec<String>) -> Self {
        Self {
            binary,
            bin_dir,
            extra_args,
        }
    }

    fn configure_command(&self, cmd: &mut Command) {
        if !self.bin_dir.as_os_str().is_empty() {
            let path_var = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = std::env::split_paths(&path_var).collect::<Vec<_>>();
            paths.insert(0, self.bin_dir.clone());
            if let Ok(new_path) = std::env::join_paths(paths) {
                cmd.env("PATH", new_path);
            }
        }
    }

    pub async fn resolve(&self, url: &str) -> Result<Value> {
        let mut cmd = Command::new(&self.binary);
        self.configure_command(&mut cmd);

        cmd.arg("-J").arg("--no-warnings").arg("--no-playlist");
        add_rate_limit_args(&mut cmd);
        cmd.args(&self.extra_args).arg("--").arg(url);

        let output = tokio::time::timeout(RESOLVE_TIMEOUT, cmd.output())
            .await
            .context("yt-dlp resolve timed out")?
            .context("failed to spawn yt-dlp")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("yt-dlp exited with {}: {}", output.status, stderr.trim());
        }

        let json: Value =
            serde_json::from_slice(&output.stdout).context("yt-dlp produced invalid JSON")?;
        Ok(json)
    }

    pub async fn stream_url(&self, format_id: &str, video_url: &str) -> Result<String> {
        let mut cmd = Command::new(&self.binary);
        self.configure_command(&mut cmd);

        // Intentionally no rate-limit args here — this runs on-demand for
        // playback and must be responsive. A failed call surfaces as a playback
        // error the user can retry; a slow call stalls the video.
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

    pub async fn list_channel(&self, channel_url: &str, limit: usize) -> Result<ChannelInfo> {
        let normalized = normalize_playlist_url(channel_url);

        // 1. Get the flat playlist JSON FIRST so we know the actual channel URL
        let mut cmd = Command::new(&self.binary);
        self.configure_command(&mut cmd);

        cmd.arg("-J")
            .arg("--flat-playlist")
            .arg("--playlist-end")
            .arg(limit.to_string())
            .arg("--no-warnings");
        add_rate_limit_args(&mut cmd);
        cmd.args(&self.extra_args).arg("--").arg(&normalized);

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

        // 2. Determine the channel URL to fetch the avatar/banner from
        let mut image_source_url = normalized.clone();
        if typ == "playlist" {
            if let Some(ch_url) = json.get("channel_url").and_then(|v| v.as_str()) {
                image_source_url = ch_url.to_string();
            } else if let Some(uploader_id) = json.get("uploader_id").and_then(|v| v.as_str()) {
                image_source_url = format!("https://www.youtube.com/channel/{}", uploader_id);
            } else if let Some(ch_id) = json.get("channel_id").and_then(|v| v.as_str()) {
                image_source_url = format!("https://www.youtube.com/channel/{}", ch_id);
            }
        }

        // 3. Get high-res channel images using the resolved channel URL
        let (mut thumbnail_url, banner_url) = self.get_channel_images(&image_source_url).await?;

        // 4. Fallback: If channel images fail (e.g. Mix playlists), use the playlist/video thumbnail
        if thumbnail_url.is_none() {
            thumbnail_url = json
                .get("thumbnails")
                .and_then(|v| v.as_array())
                .and_then(|arr| best_thumbnail(arr));
        }

        // Only use the JSON `title` for USER playlists (URLs with /playlist?list=...).
        // Channel tab URLs (/@handle/videos, /channel/UCxxx/streams, ...) also report
        // _type="playlist", but their `title` is "<Channel> - Videos" — using it
        // would create folders named "Channel - Videos" instead of the channel name.
        let is_user_playlist =
            channel_url.contains("/playlist?") || channel_url.contains("/playlist/");
        let name = if is_user_playlist {
            json.get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
        } else {
            json.get("channel")
                .and_then(|v| v.as_str())
                .or_else(|| json.get("uploader").and_then(|v| v.as_str()))
                .or_else(|| json.get("title").and_then(|v| v.as_str()))
                .unwrap_or("Unknown")
        }
        .to_string();

        let channel_id = json
            .get("channel_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

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
            banner_url,
            description,
            entries,
        })
    }

    /// Runs `yt-dlp --list-thumbnails` and parses the output for uncropped images.
    async fn get_channel_images(
        &self,
        channel_url: &str,
    ) -> Result<(Option<String>, Option<String>)> {
        let mut cmd = Command::new(&self.binary);
        self.configure_command(&mut cmd);

        cmd.arg("--list-thumbnails")
            .arg("--restrict-filenames")
            .arg("--ignore-errors")
            .arg("--no-warnings")
            .arg("--playlist-items")
            .arg("0");
        add_rate_limit_args(&mut cmd);
        cmd.args(&self.extra_args).arg("--").arg(channel_url);

        let output = tokio::time::timeout(LIST_TIMEOUT, cmd.output())
            .await
            .context("yt-dlp --list-thumbnails timed out")?
            .context("failed to spawn yt-dlp")?;

        if !output.status.success() {
            return Ok((None, None));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut poster = None;
        let mut landscape = None;

        for line in stdout.lines() {
            let line = line.trim();
            if line.contains("avatar_uncropped") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(url) = parts.last() {
                    poster = Some(url.to_string());
                }
            }
            if line.contains("banner_uncropped") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(url) = parts.last() {
                    landscape = Some(url.to_string());
                }
            }
        }

        Ok((poster, landscape))
    }

    pub async fn download_video(
        &self,
        video_id: &str,
        opts: DownloadOptions<'_>,
    ) -> Result<PathBuf> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");

        let mut cmd = Command::new(&self.binary);
        self.configure_command(&mut cmd);

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

        add_rate_limit_args(&mut cmd);
        cmd.args(&self.extra_args);
        cmd.arg("--").arg(&url);

        let output = tokio::time::timeout(DOWNLOAD_TIMEOUT, cmd.output())
            .await
            .context("yt-dlp download timed out")?
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
