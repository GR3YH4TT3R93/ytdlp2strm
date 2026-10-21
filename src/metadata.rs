use serde_json::Value;

use crate::config::MetadataLevel;
use crate::sponsorblock::Segment;
use crate::state::AppState;
use crate::ytdlp::ChannelEntry;

#[derive(Debug, Clone, Default)]
pub struct VideoMetadata {
    pub title: String,
    pub description: Option<String>,
    pub upload_date: Option<String>,
    pub duration_secs: Option<u64>,
}

pub fn from_entry(entry: &ChannelEntry) -> VideoMetadata {
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

pub async fn get_video_metadata(
    state: &AppState,
    entry: &ChannelEntry,
) -> anyhow::Result<VideoMetadata> {
    let cfg = state.config();
    if cfg.metadata.metadata_level == MetadataLevel::Full {
        let json = state
            .resolve_cache
            .get_or_resolve(&state.ytdlp, &entry.id, entry.url.as_deref())
            .await?;
        Ok(metadata_from_json(&json))
    } else {
        Ok(from_entry(entry))
    }
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub fn from_nfo(content: &str) -> VideoMetadata {
    let mut meta = VideoMetadata::default();
    let get_tag = |tag: &str| -> Option<String> {
        let start_tag = format!("<{}>", tag);
        let end_tag = format!("</{}>", tag);
        if let Some(start) = content.find(&start_tag)
            && let Some(end) = content[start..].find(&end_tag)
        {
            return Some(xml_unescape(&content[start + start_tag.len()..start + end]));
        }
        None
    };

    if let Some(title) = get_tag("title") {
        meta.title = title;
    }
    if let Some(plot) = get_tag("plot") {
        meta.description = Some(plot);
    }
    if let Some(aired) = get_tag("aired") {
        meta.upload_date = Some(aired);
    }
    if let Some(runtime_str) = get_tag("runtime")
        && let Ok(minutes) = runtime_str.parse::<u64>()
    {
        meta.duration_secs = Some(minutes * 60);
    }
    meta
}

pub fn build_episode_nfo(
    meta: &VideoMetadata,
    season: u32,
    episode: u32,
    video_id: &str,
    thumb_filename: Option<&str>,
    sponsorblock_segments: &[Segment], // <-- NEW
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

    // --- NEW: Generate Chapters for Intro Skipper ---
    if !sponsorblock_segments.is_empty() {
        let mut chapters = vec![(0.0, "Start".to_string())];

        for seg in sponsorblock_segments {
            let chapter_name = match seg.category.as_str() {
                "sponsor" => "Sponsor",
                "intro" => "Intro",
                "outro" => "Outro",
                "selfpromo" => "Selfpromo",
                "interaction" => "Interaction",
                "music_offtopic" => "Music_Offtopic",
                "preview" => "Preview",
                "filler" => "Filler",
                _ => "Skippable",
            }
            .to_string();

            // Add the start of the segment
            chapters.push((seg.start, chapter_name.clone()));

            // Add the explicit end of the segment using "Close"
            chapters.push((seg.end, format!("Close {}", chapter_name)));
        }

        // Sort chronologically so IntroSkipper sees them in the correct order
        chapters.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        out.push_str("  <chapters>\n");
        let mut last_time = -1.0;
        for (time, name) in chapters {
            // Ensure we don't write two chapters at the exact same second
            if time > last_time {
                out.push_str(&format!(
                    "    <chapter name=\"{}\" start=\"{}\" />\n",
                    name, time as u64
                ));
                last_time = time;
            }
        }
        out.push_str("  </chapters>\n");
    }
    // ------------------------------------------------

    out.push_str("</episodedetails>\n");
    out
}

pub fn build_tvshow_nfo(
    name: &str,
    description: Option<&str>,
    channel_id: Option<&str>,
    thumb_filename: Option<&str>,
    fanart_filename: Option<&str>,
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
    if let Some(fanart) = fanart_filename {
        out.push_str(&format!("  <fanart>{}</fanart>\n", xml_escape(fanart)));
    }
    out.push_str("</tvshow>\n");
    out
}
