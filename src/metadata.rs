use serde_json::Value;

use crate::config::MetadataLevel;
use crate::state::AppState;
use crate::ytdlp::ChannelEntry;

#[derive(Debug, Clone, Default)]
pub struct VideoMetadata {
    pub title: String,
    pub description: Option<String>,
    pub upload_date: Option<String>,
    pub duration_secs: Option<u64>,
}

fn from_entry(entry: &ChannelEntry) -> VideoMetadata {
    VideoMetadata {
        title: entry.title.clone(),
        description: entry.description.clone(),
        upload_date: entry.upload_date.clone(),
        duration_secs: entry.duration_secs,
    }
}

pub fn metadata_from_json(json: &Value) -> VideoMetadata {
    let upload_date = json.get("upload_date").and_then(|v| v.as_str()).map(|s| {
        if s.len() == 8 {
            format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
        } else {
            s.to_string()
        }
    });

    VideoMetadata {
        title: json
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        description: json
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        upload_date,
        duration_secs: json.get("duration").and_then(|v| v.as_u64()),
    }
}

pub async fn get_video_metadata(state: &AppState, entry: &ChannelEntry) -> VideoMetadata {
    let cfg = state.config();
    if cfg.metadata.metadata_level == MetadataLevel::Full {
        match state
            .resolve_cache
            .get_or_resolve(&state.ytdlp, &entry.id)
            .await
        {
            Ok(json) => metadata_from_json(&json),
            Err(e) => {
                tracing::warn!(
                    video_id = %entry.id,
                    error = %e,
                    "full metadata resolve failed, using basic metadata"
                );
                from_entry(entry)
            }
        }
    } else {
        from_entry(entry)
    }
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn build_episode_nfo(
    meta: &VideoMetadata,
    season: u32,
    episode: u32,
    video_id: &str,
    thumb_filename: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n");
    out.push_str("<episodedetails>\n");
    out.push_str(&format!("  <title>{}</title>\n", xml_escape(&meta.title)));
    if let Some(plot) = &meta.description {
        out.push_str(&format!("  <plot>{}</plot>\n", xml_escape(plot)));
    }
    if let Some(aired) = &meta.upload_date {
        out.push_str(&format!("  <aired>{}</aired>\n", xml_escape(aired)));
    }
    if let Some(dur) = meta.duration_secs {
        let minutes = dur / 60;
        if minutes > 0 {
            out.push_str(&format!("  <runtime>{minutes}</runtime>\n"));
        }
    }
    out.push_str(&format!("  <season>{season}</season>\n"));
    out.push_str(&format!("  <episode>{episode}</episode>\n"));
    out.push_str(&format!(
        "  <uniqueid type=\"youtube\">{}</uniqueid>\n",
        xml_escape(video_id)
    ));
    if let Some(thumb) = thumb_filename {
        out.push_str(&format!("  <thumb>{}</thumb>\n", xml_escape(thumb)));
    }
    out.push_str("</episodedetails>\n");
    out
}

pub fn build_tvshow_nfo(
    name: &str,
    description: Option<&str>,
    channel_id: Option<&str>,
    thumb_filename: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n");
    out.push_str("<tvshow>\n");
    out.push_str(&format!("  <title>{}</title>\n", xml_escape(name)));
    if let Some(plot) = description {
        out.push_str(&format!("  <plot>{}</plot>\n", xml_escape(plot)));
    }
    if let Some(id) = channel_id {
        out.push_str(&format!(
            "  <uniqueid type=\"youtube\">{}</uniqueid>\n",
            xml_escape(id)
        ));
    }
    if let Some(thumb) = thumb_filename {
        out.push_str(&format!("  <thumb>{}</thumb>\n", xml_escape(thumb)));
    }
    out.push_str("</tvshow>\n");
    out
}
