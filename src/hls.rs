use crate::sponsorblock::Segment as SbSegment;
use anyhow::{Context, Result, bail};
use m3u8_rs::{MediaPlaylist, Playlist};

pub async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client.get(url).send().await.context("fetching playlist")?;
    if !resp.status().is_success() {
        bail!("playlist fetch failed: HTTP {}", resp.status());
    }
    resp.text().await.context("reading playlist body")
}

fn resolve_uri(base: &str, uri: &str) -> Result<String> {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Ok(uri.to_string());
    }
    Ok(url::Url::parse(base)
        .with_context(|| format!("invalid base url: {base}"))?
        .join(uri)
        .with_context(|| format!("cannot resolve uri: {uri}"))?
        .to_string())
}

/// Recursively resolves a master playlist until we find the actual media playlist.
pub async fn fetch_media_playlist(
    client: &reqwest::Client,
    url: &str,
) -> Result<(MediaPlaylist, String, String)> {
    let mut current_url = url.to_string();

    loop {
        let body = fetch_text(client, &current_url).await?;
        match m3u8_rs::parse_playlist_res(body.as_bytes()) {
            Ok(Playlist::MediaPlaylist(p)) => return Ok((p, body, current_url)),
            Ok(Playlist::MasterPlaylist(m)) => {
                // Pick the highest bandwidth variant from this master
                let variant = m
                    .variants
                    .iter()
                    .max_by_key(|v| v.bandwidth)
                    .ok_or_else(|| anyhow::anyhow!("master playlist has no variants"))?;
                current_url = resolve_uri(&current_url, &variant.uri)?;
            }
            Err(e) => bail!("playlist parse failed: {e:?}"),
        }
    }
}

fn extract_map_uri(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(start) = rest.find("URI=\"") {
                let after = &rest[start + "URI=\"".len()..];
                if let Some(end) = after.find('"') {
                    return Some(after[..end].to_string());
                }
            }
        }
    }
    None
}

pub fn filter_and_rewrite(
    playlist: &MediaPlaylist,
    raw_source: &str,
    playlist_url: &str,
    skips: &[SbSegment],
    proxy_path: &str,
) -> Result<String> {
    let map_proxied = match extract_map_uri(raw_source) {
        Some(map_uri) => {
            let map_absolute = resolve_uri(playlist_url, &map_uri)?;
            let map_encoded: String =
                url::form_urlencoded::byte_serialize(map_absolute.as_bytes()).collect();
            Some(format!("{proxy_path}?u={map_encoded}"))
        }
        None => None,
    };

    let mut kept: Vec<(f64, String, bool)> = Vec::new();
    let mut discontinuity_pending = false;
    let mut t = 0.0_f64;

    for seg in &playlist.segments {
        let dur = seg.duration as f64;
        let t0 = t;
        let t1 = t + dur;
        t = t1;

        if skips.iter().any(|s| s.overlaps(t0, t1)) {
            discontinuity_pending = true;
            continue;
        }

        let absolute = resolve_uri(playlist_url, &seg.uri)?;
        let encoded: String = url::form_urlencoded::byte_serialize(absolute.as_bytes()).collect();

        kept.push((
            dur,
            format!("{proxy_path}?u={encoded}"),
            discontinuity_pending,
        ));
        discontinuity_pending = false;
    }

    let max_dur = kept.iter().map(|(d, _, _)| *d).fold(0.0_f64, f64::max);
    let target = (max_dur.ceil() as u32).max(1);

    // Use version 6 if we have an EXT-X-MAP (fMP4)
    let version = if map_proxied.is_some() { 6 } else { 3 };

    let mut out = String::from("#EXTM3U\n");
    out.push_str(&format!("#EXT-X-VERSION:{version}\n"));
    out.push_str(&format!("#EXT-X-TARGETDURATION:{target}\n"));
    out.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");

    // CRITICAL: This tag tells the player the resolution/codec of the stream.
    if let Some(map_uri) = &map_proxied {
        out.push_str(&format!("#EXT-X-MAP:URI=\"{map_uri}\"\n"));
    }

    let mut first_segment = true;
    for (dur, uri, discontinuity) in &kept {
        if *discontinuity && !first_segment {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        out.push_str(&format!("#EXTINF:{dur:.3},\n{uri}\n"));
        first_segment = false;
    }
    out.push_str("#EXT-X-ENDLIST\n");
    Ok(out)
}
