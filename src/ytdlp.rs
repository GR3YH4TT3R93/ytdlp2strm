use anyhow::{Context, Result, bail};
use chrono::DateTime;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(3600);

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
    pub url: Option<String>,
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

#[derive(Debug, Clone)]
pub struct LiveStreamInfo {
    pub title: String,
    pub stream_url: String,
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

        let target_url = if video_url.starts_with("http") {
            video_url.to_string()
        } else if video_url.starts_with('v') && video_url.len() > 5 {
            format!("https://www.twitch.tv/videos/{}", &video_url[1..])
        } else {
            format!("https://www.youtube.com/watch?v={}", video_url)
        };

        cmd.arg("-g")
            .arg("-f")
            .arg(format_id)
            .arg("--no-warnings")
            .arg("--no-playlist")
            .args(&self.extra_args)
            .arg("--")
            .arg(&target_url);

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

    pub async fn check_twitch_live(&self, channel_url: &str) -> Result<Option<LiveStreamInfo>> {
        let mut cmd = Command::new(&self.binary);
        self.configure_command(&mut cmd);

        cmd.arg("--no-warnings")
            .arg("--print")
            .arg("%(is_live)s")
            .arg("--print")
            .arg("%(title)s")
            .arg("--print")
            .arg("%(webpage_url)s")
            .args(&self.extra_args)
            .arg("--")
            .arg(channel_url);

        let output = tokio::time::timeout(RESOLVE_TIMEOUT, cmd.output())
            .await
            .context("yt-dlp live check timed out")?
            .context("failed to spawn yt-dlp")?;

        if !output.status.success() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();

        if lines.len() >= 3 && lines[0].trim() == "True" {
            Ok(Some(LiveStreamInfo {
                title: lines[1].trim().to_string(),
                stream_url: lines[2].trim().to_string(),
            }))
        } else {
            Ok(None)
        }
    }

    /// Uses Twitch's GraphQL API to fetch the highest quality avatar and banner.
    async fn fetch_twitch_images(
        &self,
        channel_name: &str,
    ) -> Result<(Option<String>, Option<String>)> {
        let client = reqwest::Client::new();

        // This is the exact query and hash the Python ytdlp2STRM script uses.
        let payload = json!([{
            "operationName": "ChannelShell",
            "variables": { "login": channel_name },
            "extensions": {
                "persistedQuery": {
                    "version": 1,
                    "sha256Hash": "580ab410bcd0c1ad194224957ae2241e5d252b2c5173d8e0cce9d32d5bb14efe"
                }
            }
        }]);

        let resp = client
            .post("https://gql.twitch.tv/gql")
            .header("Client-Id", "kimne78kx3ncx6brgo4mv6wki5h1ko")
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await;

        if let Ok(resp) = resp {
            if let Ok(json) = resp.json::<Value>().await {
                let user = &json[0]["data"]["userOrError"];

                let mut avatar = user["profileImageURL"].as_str().map(|s| s.to_string());
                let mut banner = user["bannerImageURL"].as_str().map(|s| s.to_string());

                // Twitch avatars default to 70x70 or 300x300.
                // We string-replace to 600x600 to get the absolute maximum resolution.
                if let Some(a) = &avatar {
                    avatar = Some(a.replace("70x70", "600x600").replace("300x300", "600x600"));
                }

                // If there's no banner, don't use the generic fallback URL Twitch provides
                if let Some(b) = &banner {
                    if b.contains("no_banner") {
                        banner = None;
                    }
                }

                return Ok((avatar, banner));
            }
        }

        Ok((None, None))
    }

    pub async fn list_channel(&self, channel_url: &str, limit: usize) -> Result<ChannelInfo> {
        let normalized = normalize_playlist_url(channel_url);

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

        let is_twitch = channel_url.contains("twitch.tv");

        let name = if is_twitch {
            let from_json = json
                .get("playlist_channel")
                .and_then(|v| v.as_str())
                .or_else(|| json.get("uploader").and_then(|v| v.as_str()))
                .or_else(|| json.get("channel").and_then(|v| v.as_str()))
                .or_else(|| json.get("id").and_then(|v| v.as_str()));

            if let Some(n) = from_json {
                n.to_string()
            } else {
                let clean_url = channel_url.split('?').next().unwrap_or(channel_url);
                let parts: Vec<&str> = clean_url.split('/').filter(|s| !s.is_empty()).collect();
                if parts.len() >= 2 {
                    let candidate = parts[1];
                    if candidate != "videos" && !candidate.starts_with('v') {
                        candidate.to_string()
                    } else {
                        "Unknown Twitch Channel".to_string()
                    }
                } else {
                    "Unknown Twitch Channel".to_string()
                }
            }
        } else if channel_url.contains("/playlist?") || channel_url.contains("/playlist/") {
            json.get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string()
        } else {
            json.get("channel")
                .and_then(|v| v.as_str())
                .or_else(|| json.get("uploader").and_then(|v| v.as_str()))
                .or_else(|| json.get("title").and_then(|v| v.as_str()))
                .unwrap_or("Unknown")
                .to_string()
        };

        let (mut thumbnail_url, banner_url) = if is_twitch {
            let (avatar, banner) = self
                .fetch_twitch_images(&name)
                .await
                .unwrap_or((None, None));
            (avatar, banner)
        } else {
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
            self.get_channel_images(&image_source_url).await?
        };

        if thumbnail_url.is_none() {
            thumbnail_url = json
                .get("thumbnails")
                .and_then(|v| v.as_array())
                .and_then(|arr| best_thumbnail(arr))
                .or_else(|| {
                    json.get("thumbnail")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });

            if thumbnail_url.is_none() && is_twitch {
                if let Some(first_entry) = json
                    .get("entries")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                {
                    thumbnail_url = first_entry
                        .get("thumbnails")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| best_thumbnail(arr))
                        .or_else(|| {
                            first_entry
                                .get("thumbnail")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        });
                }
            }
        }

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

                        let url = e.get("url").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let title = e
                            .get("title")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .unwrap_or(&id)
                            .to_string();

                        let mut thumbnail_url = e
                            .get("thumbnails")
                            .and_then(|v| v.as_array())
                            .and_then(|arr| best_thumbnail(arr))
                            .or_else(|| {
                                e.get("thumbnail")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                            });

                        // UPGRADE VOD THUMBNAIL: Twitch returns 320x180 by default, replace with 1280x720
                        if let Some(t) = &thumbnail_url {
                            if t.contains("thumb0-320x180.jpg") {
                                thumbnail_url = Some(t.replace("320x180", "1280x720"));
                            }
                        }

                        let duration_secs = e.get("duration").and_then(|v| v.as_u64());

                        let upload_date = e
                            .get("upload_date")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| {
                                e.get("timestamp").and_then(|v| v.as_i64()).map(|ts| {
                                    if let Some(dt) = DateTime::from_timestamp(ts, 0) {
                                        dt.format("%Y%m%d").to_string()
                                    } else {
                                        "".to_string()
                                    }
                                })
                            });

                        let description = e
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        Some(ChannelEntry {
                            id,
                            url,
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
            if line.contains("avatar_uncropped") || line.contains("profile") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(url) = parts.last() {
                    poster = Some(url.to_string());
                }
            }
            if line.contains("banner_uncropped") || line.contains("banner") {
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
        let url = if video_id.starts_with("http") {
            video_id.to_string()
        } else if video_id.starts_with('v') && video_id.len() > 5 {
            format!("https://www.twitch.tv/videos/{}", &video_id[1..])
        } else {
            format!("https://www.youtube.com/watch?v={}", video_id)
        };

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
        cmd.args(&self.extra_args).arg("--").arg(&url);

        let output = tokio::time::timeout(DOWNLOAD_TIMEOUT, cmd.output())
            .await
            .context("yt-dlp download timed out")?
            .context("failed to spawn yt-dlp")?;

        if !output.status.success() {
            bail!(
                "yt-dlp download failed (exit {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
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
    let lower = url.to_lowercase();
    if lower.contains("twitch.tv") {
        if lower.contains("/videos/")
            || lower.contains("/video/")
            || lower.ends_with("/videos")
            || lower.ends_with("/clips")
        {
            return url.to_string();
        }
        let base = url.trim_end_matches('/');
        return format!("{base}/videos");
    }
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
