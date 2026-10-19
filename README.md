# ytdlp2strm

A Rust rewrite of [ytdlp2STRM](https://github.com/fe80Grau/ytdlp2STRM) that goes further: **SponsorBlock-filtered HLS streaming without downloading**, alongside the original download-to-disk workflow.

Point it at YouTube channels or playlists. It generates a Jellyfin/Emby/Kodi-compatible library of `.strm` files, NFO metadata, and thumbnails — and serves the actual media through two distinct pipelines:

- **Stream mode:** An on-the-fly HLS proxy that removes SponsorBlock segments from the playlist before the player ever sees them. No disk usage, no download wait, plays immediately. This is the feature the original project explicitly can't do.
- **Download mode:** yt-dlp downloads the video, physically cuts SponsorBlock segments with ffmpeg, caches to disk, and serves the file with full HTTP Range support.

Mode is per-scan, so a single server can host some channels in stream mode and others in download mode simultaneously.

## Features

### Web UI

- **Dashboard:** Built-in, responsive web interface served at the root path (`/`).
- **Live Config Editing:** Edit `config.toml` directly in the browser. A structured form handles common settings, and an "Advanced" tab allows raw TOML editing. Changes are hot-reloaded instantly without restarting the server.
- **Live Scan Logs:** Trigger scans from the UI and watch the output stream in real-time.
- **Auto-Scheduling:** Toggle a checkbox during a quick scan to permanently add the channel/playlist to `config.toml` for background cron refreshes.

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

- **Channel and playlist scanning:** Triggered via `POST /scan` or the Web UI. The server auto-detects whether the URL is a channel or playlist and handles it accordingly.
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

*Note: If `yt-dlp` or `ffmpeg` are missing from your system's `$PATH`, the server will automatically download portable binaries into the `./bin` directory on first startup.*

Install the tools manually (recommended):

```bash
# Arch
sudo pacman -S yt-dlp ffmpeg

# Debian / Ubuntu
sudo apt install yt-dlp ffmpeg

# macOS
brew install yt-dlp ffmpeg
```

## Installation

```bash
git clone https://github.com/yourname/ytdlp2strm
cd ytdlp2strm
cargo build --release
```

The binary is at target/release/ytdlp2strm. Copy it anywhere on your $PATH or run it directly.

## Configuration

ytdlp2strm reads a TOML file specified by --config (defaults to ./config.toml).

The server resolves paths based on standard OS precedence:

1. --config flag
2. ./config.toml (Portable/Current Dir)
3. ~/.config/ytdlp2strm/config.toml (XDG)
4. /etc/ytdlp2strm/config.toml (System-wide)

If no config is found, a commented default template is generated based on your user privileges.

All sections except [server] have sensible defaults — the file below shows every option with its default value.

```toml
[server]
# Accepts either an IP:port or a full URL.
#   - IP:port binds there and uses http://<ip>:<port> as the base URL.
#   - URL binds to 0.0.0.0 on the port from the URL (or 8080) and uses the
#     URL as-is for link generation. Use this behind a reverse proxy.
address = "127.0.0.1:8080"

[auth]
# HTTP Basic auth. When username is empty, auth is disabled.
# Protects the Web UI, /scan, /strm, /debug/*, /schedule/trigger, /channels.
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

[youtube]
# Extra flags appended to every yt-dlp invocation. See `yt-dlp --help`.
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
pre_download = false
# Max concurrent yt-dlp download processes.
concurrency = 3

[metadata]
# Generate NFO sidecar files.
write_nfo = true
# Download channel posters and video thumbnails.
write_thumbnails = true
# "full"  — resolve each video individually for description, upload date.
# "basic" — use flat-playlist data only; ~10× faster but NFOs lack plot
metadata_level = "full"

[state]
# Where scheduler state persists between restarts. Empty disables persistence.
schedule_file = "./state/schedule.json"

[deps]
# Where to store the portable yt-dlp and ffmpeg binaries if auto-downloaded.
bin_dir = "./bin"
auto_download = true

[schedule]
# Cron expression for automatic refresh. 5-field or 6-field (with seconds).
# Empty disables the scheduler.
refresh_cron = "0 */6 * * *"
# Sleep between sources so a large library doesn't hammer YouTube.
per_source_delay_seconds = 5

# Add channels or playlists to keep refreshed. Each entry is scanned on
# every cron tick.
sources = [
    # { url = "https://www.youtube.com/@mkbhd", limit = 50 },
    # { url = "https://www.youtube.com/playlist?list=PLxxxxxxxxxxxxxxxx", limit = 100 }
]
```

## Hot-Reloading

When editing the configuration via the Web UI, changes are applied to the running server immediately. If you change an output_dir (like moving strm from ./strm to /mnt/nas/strm), the server will automatically move the existing files to the new location in the background without restarting.

Usage

Start the server

```bash
./target/release/ytdlp2strm --config config.toml
```

Or with debug logging for troubleshooting:

```bash
RUST_LOG=ytdlp2strm=debug ./target/release/ytdlp2strm
```

## Web UI

Navigate to <http://127.0.0.1:8080/> in your browser. If you have [auth] username set in the config, your browser will prompt for those credentials.

- Settings Form: Edit server, paths, SponsorBlock, and security settings using a clean graphical interface.
- Advanced (TOML): Edit the raw config.toml file directly (preserves comments).
- Quick Scan: Trigger a scan for a new channel or playlist and watch the output in real-time.

Scan a channel (CLI)

```bash
curl -u admin:password -X POST http://127.0.0.1:8080/scan \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://www.youtube.com/@mkbhd","limit":50}'
```

Options:

- "limit": 50 — enumerate top N videos (max 1000, default 100)
- "force": true — rewrite every file regardless of whether it exists
- "mode": "download" — override [strm] mode for this scan only
- "add_to_schedule": true — append this URL to config.toml for cron refreshes

Trigger scheduled refresh manually

```bash
curl -u admin:password -X POST http://127.0.0.1:8080/schedule/trigger
```

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

HTTP API

Public Endpoints (No Auth)

- GET /health — Liveness probe, returns ok
- GET /metrics — Prometheus text format
- GET /play/{video_id} — Stream-mode HLS bridge master playlist
- GET /download/{video_id} — Download-mode file serve (with Range support)
- GET /bridge/{video_id}/master.m3u8 — Alias for /play/{video_id}
- GET /bridge/{video_id}/video.m3u8 — Filtered video media playlist
- GET /bridge/{video_id}/audio.m3u8 — Filtered audio media playlist
- GET /bridge/{video_id}/segment — HLS segment proxy (used internally)
- GET /schedule/status — Schedule status snapshot

Admin Endpoints (Requires Auth)

- GET / — Web UI dashboard
- GET /api/config — Get current config (JSON & TOML)
- POST /api/config — Update config (Hot-reload)
- POST /scan — Scan a channel or playlist (blocks until complete)
- POST /scan/background — Scan in the background (used by UI)
- GET /scan/logs — Poll live logs from the background scan
- POST /schedule/trigger — Trigger scheduled refresh
- GET /channels — Library inventory (channels, file counts, sizes)
- GET /strm/{video_id} — Generate STRM+NFO+thumb for one video

When [auth] username is empty, all routes are open.

## Operations

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
ExecStart=/usr/local/bin/ytdlp2strm --config /etc/ytdlp2strm/config.toml
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

## Architecture

```
src/
├── main.rs           Bootstrap and route table
├── config.rs         TOML schema and address parsing
├── state.rs          AppState, RwLock, resolve cache, metrics
├── auth.rs           HTTP Basic middleware
├── bridge.rs         HLS bridge: master, media, segment proxy
├── hls.rs            Playlist parse/filter/rewrite
├── downloads.rs      yt-dlp download + HTTP file serve
├── sponsorblock.rs   SponsorBlock API client
├── metadata.rs       NFO builders, per-video metadata
├── images.rs         Image sniff, download, format cleanup
├── files.rs          STRM writes, per-ID file indexing, sanitize
├── scan.rs           Channel/playlist scan pipeline, live logs
├── schedule.rs       Cron scheduler, refresh orchestration
├── handlers.rs       Web UI, Config API, debug endpoints, /channels
├── deps.rs           Portable yt-dlp/ffmpeg resolver
├── ui.html           Embedded web dashboard (vanilla JS/CSS)
└── ytdlp.rs          yt-dlp subprocess wrapper
```

## Known limitations

- No live stream support: Live streams aren't handled specially and will likely fail or produce incomplete output.
- No HLS encryption support: The bridge assumes unencrypted HLS.
- Range requests over HLS: /bridge/*/segment forwards the client's Range header, but YouTube's CDN typically ignores Range on .ts segments.

## License

MIT.

## Acknowledgements

- Original concept: [fe80Grau/ytdlp2STRM](https://github.com/fe80Grau/ytdlp2STRM)
- SponsorBlock: [ajayyy/SponsorBlock](https://github.com/ajayyy/SponsorBlock)
- yt-dlp: [yt-dlp/yt-dlp](https://github.com/yt-dlp/yt-dlp)
