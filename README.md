# ytdlp2strm

A Rust rewrite of [ytdlp2STRM](https://github.com/fe80Grau/ytdlp2STRM) that goes further: **SponsorBlock-filtered HLS streaming without downloading**, alongside the original download-to-disk workflow.

Point it at YouTube channels or playlists. It generates a Jellyfin/Emby/Kodi-compatible library of `.strm` files, NFO metadata, and thumbnails — and serves the actual media through two distinct pipelines:

- **Stream mode:** An on-the-fly HLS proxy that removes SponsorBlock segments from the playlist before the player ever sees them. No disk usage, no download wait, plays immediately. This is the feature the original project explicitly can't do.
- **Download mode:** yt-dlp downloads the video, physically cuts SponsorBlock segments with ffmpeg, caches to disk, and serves the file with full HTTP Range support.

Mode is per-scan, so a single server can host some channels in stream mode and others in download mode simultaneously.

## Features

### Media pipelines

Both endpoints always work regardless of scan mode — mode only controls what URL goes into each `.strm` file.

**Stream Mode (`/play/{id}`)**

- **Latency:** ~2s (playlist fetch + SponsorBlock API).
- **SponsorBlock:** Segments are removed from the HLS playlist; the player skips them on the fly.
- **Disk usage:** Zero. The bridge is completely stateless.
- **Seeking:** If you seek to a sponsor segment, the player jumps over it to the next real content.
- **Client compatibility:** Requires an HLS-capable player (mpv, VLC, Jellyfin, Infuse, web browsers).
- **Re-scan cost:** Zero.

**Download Mode (`/download/{id}`)**

- **Latency:** 15–90s on first play (full yt-dlp download); instant on subsequent plays.
- **SponsorBlock:** Segments are physically cut from the MP4 file using ffmpeg.
- **Disk usage:** ~50–500 MB per video. Features automatic LRU-cleanup.
- **Seeking:** The sponsor content simply doesn't exist in the file.
- **Client compatibility:** Works with any video player.
- **Re-scan cost:** Zero (served from disk cache).

### Library generation

- **Channel and playlist scanning:** Triggered via `POST /scan`. The server auto-detects whether the URL is a channel or playlist and handles it accordingly.
- **NFO metadata:** Generates `tvshow.nfo` per channel and `<title>.nfo` per episode. Includes title, plot, air date, runtime, and a stable `uniqueid type="youtube"` for watch-state tracking.
- **Thumbnails:** Downloads channel `poster.{jpg,webp,png}` and per-episode `<title>-thumb.{jpg,webp,png}`. The file extension is sniffed from the magic bytes so the filename always matches the actual content.
- **Idempotent scans:** Existing files are skipped unless their content changed. Cron refreshes are fast (15s for 100 videos when nothing is new, vs 3+ minutes on first scan).
- **Self-healing:** NFO `<thumb>` references and STRM URLs are validated on each scan. Stale references are rewritten without re-downloading media.

### Operations

- **Scheduled refresh:** Via cron (5-field or 6-field syntax), with persisted last-run status across restarts.
- **Security:** HTTP Basic auth on admin routes; media routes stay open for players.
- **Observability:** Prometheus metrics at `/metrics`.
- **Flexible bind address:** `127.0.0.1:8080`, `0.0.0.0:5000`, or `https://ytdlp.example.com` in a single config field.
- **Cache management:** Configurable retention (`retention_days`), concurrent download limits, and optional pre-download during scans.
- **Multi-quality master playlists:** Stream mode advertises every available resolution; players pick based on bandwidth and screen.
- **Codec preference:** Prefers AVC over VP9 for maximum player compatibility when AVC HLS variants exist.

## Requirements

- **Rust** 1.85+ (edition 2024)
- **yt-dlp** on `$PATH` (2024+ recommended for reliable HLS output)
- **ffmpeg** on `$PATH` (required for download mode)
- **A media server** (optional) — Jellyfin, Emby, Kodi, or Plex if you want a library UI

Install the tools:

```bash
# Arch
sudo pacman -S yt-dlp ffmpeg

# Debian / Ubuntu
sudo apt install yt-dlp ffmpeg

# macOS
brew install yt-dlp ffmpeg
```

Installation

```bash
git clone https://github.com/yourname/ytdlp2strm
cd ytdlp2strm
cargo build --release
```

The binary is at target/release/ytdlp2strm. Copy it anywhere on your $PATH or run it directly.

Configuration

ytdlp2strm reads a TOML file specified by --config (defaults to ./config.toml). All sections except [server] have sensible defaults — the file below shows every option with its default value.

```toml
[server]
# Accepts either an IP:port or a full URL.
#   - IP:port binds there and uses http://<ip>:<port> as the base URL.
#   - URL binds to 0.0.0.0 on the port from the URL (or 8080) and uses the
#     URL as-is for link generation. Use this behind a reverse proxy.
address = "127.0.0.1:8080"

[auth]
# HTTP Basic auth. When username is empty, auth is disabled.
# Protects /scan, /strm, /debug/*, /schedule/trigger, /channels.
# Leaves /play, /download, /bridge, /health, /metrics, /schedule/status open.
username = ""
password = ""

[sponsorblock]
enabled = true
# "all"     — every category including filler and poi_highlight
# "default" — sponsor, selfpromo, interaction, intro, outro, preview, music_offtopic
# "none"    — disable
# Or a comma-separated list: "sponsor,selfpromo,intro"
remove = "default"
# Optional custom SponsorBlock instance
# api = "https://sponsor.ajay.cloud"

[youtube]
# Extra flags appended to every yt-dlp invocation. See `yt-dlp --help`.
# Examples: ["--cookies", "/path/to/cookies.txt"], ["--proxy", "socks5://..."]
extra_args = []

[strm]
# Where .strm/.nfo/thumbnail files are written. Created on first run.
output_dir = "./strm"
# Default mode for scans and /strm/{id}. Per-scan override available.
#   "stream"   — .strm contains /play/{id}, HLS bridge with SB filtering
#   "download" — .strm contains /download/{id}, cached on disk
mode = "stream"

[downloads]
# Cache directory for download-mode videos. Created on first run.
output_dir = "./downloads"
# Delete cached files older than N days. 0 = keep forever.
retention_days = 30
# On download-mode scans, pre-fetch videos in the background.
# false = first play of each video waits for the download.
# true  = scans return immediately, downloads proceed at up to
#         `concurrency` in parallel.
pre_download = false
# Max concurrent yt-dlp download processes.
concurrency = 3

[metadata]
# Generate NFO sidecar files.
write_nfo = true
# Download channel posters and video thumbnails.
write_thumbnails = true
# "full"  — resolve each video individually for description, upload date,
#           precise runtime (~1–2s per video during scan)
# "basic" — use flat-playlist data only; ~10× faster but NFOs lack plot
metadata_level = "full"

[state]
# Where scheduler state persists between restarts. Empty disables persistence.
schedule_file = "./state/schedule.json"

[schedule]
# Cron expression for automatic refresh. 5-field or 6-field (with seconds).
# Empty disables the scheduler.
refresh_cron = "0 */6 * * *"
# Sleep between sources so a large library doesn't hammer YouTube.
per_source_delay_seconds = 5

# Add channels or playlists to keep refreshed. Each entry is scanned on
# every cron tick. Comment or remove to disable auto-refresh of a source.
[[schedule.sources]]
url = "https://www.youtube.com/@mkbhd"
limit = 50

[[schedule.sources]]
url = "https://www.youtube.com/playlist?list=PLxxxxxxxxxxxxxxxx"
limit = 100
```

Bind address forms

```toml
[server]
address = "127.0.0.1:8080"              # local only
address = "0.0.0.0:8080"                # all interfaces, http://<any-ip>:8080
address = "192.168.1.10:5000"           # specific interface
address = "https://ytdlp.example.com"   # behind reverse proxy; binds 0.0.0.0:8080
```

The URL form is required when running behind Caddy, nginx, Traefik, or Cloudflare Tunnel — the URL is what gets embedded in .strm files so players can reach the server from outside the network.

Usage

Start the server

```bash
./target/release/ytdlp2strm --config config.toml
```

Or with debug logging for troubleshooting:

```bash
RUST_LOG=ytdlp2strm=debug ./target/release/ytdlp2strm
```

Scan a channel

```bash
curl -u admin:password -X POST http://127.0.0.1:8080/scan \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://www.youtube.com/@mkbhd","limit":50}'
```

Response:

```json
{
  "channel": "Marques Brownlee",
  "channel_dir": "./strm/Marques Brownlee",
  "mode": "stream",
  "total": 50,
  "strm_written": 50,
  "strm_skipped": 0,
  "nfo_written": 50,
  "nfo_skipped": 0,
  "thumb_written": 50,
  "thumb_skipped": 0,
  "channel_nfo_written": 1,
  "channel_poster_written": 1,
  "failed": 0,
  "errors": []
}
```

Options:

· "limit": 50 — enumerate top N videos (max 1000, default 100)
· "force": true — rewrite every file regardless of whether it exists
· "mode": "download" — override [strm] mode for this scan only

A bare channel URL like @mkbhd is auto-normalized to @mkbhd/videos — the channel home tab only returns ~3 featured videos, not the full catalog. URLs with explicit tabs (/videos, /streams, /shorts, /playlists, /community) are passed through unchanged.

Scan a playlist

```bash
curl -u admin:password -X POST http://127.0.0.1:8080/scan \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://www.youtube.com/playlist?list=PLxxxxxxxx","limit":100}'
```

Identical code path. Playlist URLs pass through without the /videos normalization.

Play a video directly

```bash
# Stream (HLS bridge with SponsorBlock filtering)
mpv http://127.0.0.1:8080/play/dQw4w9WgXcQ

# Download then serve (physical SB removal, cached)
mpv http://127.0.0.1:8080/download/dQw4w9WgXcQ
```

Trigger scheduled refresh manually

```bash
curl -u admin:password -X POST http://127.0.0.1:8080/schedule/trigger
```

Runs the same refresh the cron job would. Returns 202 Accepted if started, 409 Conflict if a refresh is already running.

Media server integration (Jellyfin)

The folder layout produced by scans is directly consumable as a Jellyfin TV Shows library.

Setup

1. In Jellyfin: Dashboard → Libraries → Add Media Library
2. Content type: Shows
3. Folder: <absolute path to your strm output dir>
4. Save and trigger a library scan.

Expected layout

```
strm/
├── Marques Brownlee/
│   ├── poster.webp
│   ├── tvshow.nfo
│   ├── AirPods Pro 3 Review [GB0b6KFZVq0].strm
│   ├── AirPods Pro 3 Review [GB0b6KFZVq0].nfo
│   ├── AirPods Pro 3 Review [GB0b6KFZVq0]-thumb.webp
│   └── ...
└── The Marxist Project/
    ├── poster.jpg
    ├── tvshow.nfo
    └── ...
```

Each channel appears as a show. Each video appears as an episode in season 1. uniqueid type="youtube" in the NFO keeps watch state and progress tracking stable across re-scans.

SponsorBlock

SponsorBlock integration works differently in each mode:

Stream mode — segments skipped in the playlist

ytdlp2strm fetches the video's HLS media playlist from YouTube, then removes every segment whose time range overlaps a SponsorBlock interval. The rewritten playlist is served to the player, which never sees the sponsor segments.

Limitation: A segment that overlaps a SponsorBlock interval is removed entirely, even if only the start or end of that segment is sponsored. With YouTube's variable segment lengths (3–7s), the loss is typically a couple of seconds per boundary.

Download mode — segments physically cut from the file

yt-dlp downloads the video with --sponsorblock-remove <categories>, which uses ffmpeg to strip the sponsored segments before writing the output file. The cached video has no sponsor content at all.

HTTP API

Public Endpoints (No Auth)

· GET /health — Liveness probe, returns ok
· GET /metrics — Prometheus text format
· GET /play/{video_id} — Stream-mode HLS bridge master playlist
· GET /download/{video_id} — Download-mode file serve (with Range support)
· GET /bridge/{video_id}/master.m3u8 — Alias for /play/{video_id}
· GET /bridge/{video_id}/video.m3u8 — Filtered video media playlist
· GET /bridge/{video_id}/audio.m3u8 — Filtered audio media playlist
· GET /bridge/{video_id}/segment — HLS segment proxy (used internally)
· GET /schedule/status — Schedule status snapshot

Admin Endpoints (Requires Auth)

· GET /debug/resolve/{video_id} — Raw yt-dlp JSON summary
· GET /debug/sponsorblock/{video_id} — Raw SponsorBlock segments
· GET /strm/{video_id} — Generate STRM+NFO+thumb for one video
· POST /scan — Scan a channel or playlist
· POST /schedule/trigger — Trigger scheduled refresh
· GET /channels — Library inventory (channels, file counts, sizes)

When [auth] username is empty, all routes are open.

Operations

Running as a systemd service

```ini
# /etc/systemd/system/ytdlp2strm.service
[Unit]
Description=ytdlp2strm
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ytdlp2strm
Group=ytdlp2strm
WorkingDirectory=/var/lib/ytdlp2strm
ExecStart=/usr/local/bin/ytdlp2strm --config /var/lib/ytdlp2strm/config.toml
Restart=on-failure
RestartSec=10
Environment="RUST_LOG=ytdlp2strm=info"

[Install]
WantedBy=multi-user.target
```

Behind a reverse proxy

Use the URL form in [server] so .strm files contain the right public URL:

```toml
[server]
address = "https://ytdlp.example.com"
```

Caddy example:

```
ytdlp.example.com {
    reverse_proxy 127.0.0.1:8080 {
        flush_interval -1
    }
}
```

flush_interval -1 disables response buffering — important for HLS segment streaming.

Nginx example:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_buffering off;
    proxy_read_timeout 3600;
}
```

proxy_buffering off is required for segment streaming.

Troubleshooting

· Scan returns total: 3 for a channel: The channel home tab only returns featured videos. The server normalizes bare handles to /videos automatically; if you already passed @handle, this should not happen.
· Player reports "no moov before mdat": The #EXT-X-MAP init segment is missing from the media playlist. Verify with curl <http://host:port/bridge/{id}/video.m3u8> | head -5 — line 5 should be #EXT-X-MAP:URI="...".
· VLC fails with "Codec h264 not supported": Local VLC plugin issue. Verify with ffplay — if it plays, install vlc-plugins-all (Arch) or vlc-plugin-ffmpeg (Debian) and try again. Or switch to mpv.
· Downloads keep failing: Run with RUST_LOG=ytdlp2strm=debug and check the log. Common causes: outdated yt-dlp (yt-dlp -U), rate-limiting (add extra_args = ["--sleep-requests", "1"]), or missing ffmpeg.
· Thumbnails broken in Jellyfin: Check file ./strm/*/poster.*. If extension mismatches actual bytes, delete and rescan. Modern builds name files correctly via magic-byte sniffing.

Architecture

```
src/
├── main.rs           Bootstrap and route table (~150 lines)
├── config.rs         TOML schema and address parsing
├── state.rs          AppState, resolve cache, metrics
├── auth.rs           HTTP Basic middleware
├── bridge/           HLS bridge: master, media, segment proxy
├── hls.rs            Playlist parse/filter/rewrite
├── downloads.rs      yt-dlp download + HTTP file serve
├── sponsorblock.rs   SponsorBlock API client
├── metadata.rs       NFO builders, per-video metadata
├── images.rs         Image sniff, download, format cleanup
├── files.rs          STRM writes, per-ID file indexing, filename sanitize
├── scan.rs           Channel/playlist scan pipeline
├── schedule.rs       Cron scheduler, refresh orchestration
├── handlers.rs       Debug endpoints, single-video STRM, /channels
└── ytdlp.rs          yt-dlp subprocess wrapper
```

Known limitations

· No live stream support: Live streams aren't handled specially and will likely fail or produce incomplete output.
· No HLS encryption support: The bridge assumes unencrypted HLS.
· Range requests over HLS: /bridge/*/segment forwards the client's Range header, but YouTube's CDN typically ignores Range on .ts segments.

License

MIT.

Acknowledgements

· Original concept: fe80Grau/ytdlp2STRM

· SponsorBlock: ajayyy/SponsorBlock

· yt-dlp: yt-dlp/yt-dlp
