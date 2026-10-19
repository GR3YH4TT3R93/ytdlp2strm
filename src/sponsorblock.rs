use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone)]
pub struct SponsorBlock {
    client: reqwest::Client,
    api_base: String,
    categories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawSegment {
    #[serde(rename = "UUID")]
    #[allow(dead_code)]
    uuid: String,
    category: String,
    segment: [f64; 2],
    #[serde(rename = "actionType", default)]
    action_type: Option<String>,
}

/// A single skip interval, in seconds.
#[derive(Debug, Clone)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub category: String,
}

impl Segment {
    /// True if this segment overlaps the half-open interval `[t0, t1)`.
    pub fn overlaps(&self, t0: f64, t1: f64) -> bool {
        t0 < self.end && t1 > self.start
    }
}

impl SponsorBlock {
    pub fn new(categories: Vec<String>, api_base: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("ytdlp2strm/0.1 (+https://github.com/yourname/ytdlp2strm)")
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .build()
                .expect("sponsorblock HTTP client"),
            api_base: api_base.unwrap_or_else(|| "https://sponsor.ajay.app".to_string()),
            categories,
        }
    }

    pub fn categories(&self) -> &[String] {
        &self.categories
    }

    pub fn is_enabled(&self) -> bool {
        !self.categories.is_empty()
    }

    /// Fetch skip segments for a video. Returns an empty Vec if SponsorBlock
    /// has no data (404) — common for new, niche, or unsubmitted videos.
    pub async fn fetch(&self, video_id: &str) -> Result<Vec<Segment>> {
        if !self.is_enabled() {
            return Ok(Vec::new());
        }

        let cats_json = serde_json::to_string(&self.categories)?;

        let mut url = Url::parse(&format!(
            "{}/api/skipSegments",
            self.api_base.trim_end_matches('/')
        ))
        .context("invalid SponsorBlock api_base")?;

        url.query_pairs_mut()
            .append_pair("videoID", video_id)
            .append_pair("categories", &cats_json);

        let resp = self
            .client
            .get(url)
            .send()
            .await
            .context("SponsorBlock request failed")?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("SponsorBlock returned HTTP {status}: {}", body.trim());
        }

        let bytes = resp.bytes().await.context("reading SponsorBlock body")?;
        let raw: Vec<RawSegment> = serde_json::from_slice(&bytes).map_err(|e| {
            anyhow::anyhow!(
                "SponsorBlock invalid JSON: {e}\nBody: {}",
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(1000)
                    .collect::<String>()
            )
        })?;

        Ok(raw
            .into_iter()
            .filter(|s| s.action_type.as_deref().unwrap_or("skip") == "skip")
            .map(|s| Segment {
                start: s.segment[0],
                end: s.segment[1],
                category: s.category,
            })
            .collect())
    }
}
