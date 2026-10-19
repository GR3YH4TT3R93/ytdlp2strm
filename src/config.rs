use serde::{Deserialize, Deserializer};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use url::Url;

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub sponsorblock: SponsorBlockConfig,
    #[serde(default)]
    pub youtube: YouTubeConfig,
    #[serde(default)]
    pub strm: StrmConfig,
    #[serde(default)]
    pub state: StateConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub address: BindAddress,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct AuthConfig {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

impl AuthConfig {
    pub fn is_enabled(&self) -> bool {
        !self.username.is_empty()
    }
}

#[derive(Debug, Deserialize)]
pub struct SponsorBlockConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_sb_remove")]
    pub remove: String,
    #[serde(default)]
    pub api: Option<String>,
}

fn default_sb_remove() -> String {
    "default".to_string()
}

impl Default for SponsorBlockConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            remove: default_sb_remove(),
            api: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct YouTubeConfig {
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct StrmConfig {
    #[serde(default = "default_strm_dir")]
    pub output_dir: PathBuf,
}

fn default_strm_dir() -> PathBuf {
    PathBuf::from("./strm")
}

impl Default for StrmConfig {
    fn default() -> Self {
        Self {
            output_dir: default_strm_dir(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct StateConfig {
    #[serde(default = "default_schedule_file")]
    pub schedule_file: PathBuf,
}

fn default_schedule_file() -> PathBuf {
    PathBuf::from("./state/schedule.json")
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            schedule_file: default_schedule_file(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ScheduleConfig {
    #[serde(default = "default_refresh_cron")]
    pub refresh_cron: String,
    #[serde(default = "default_per_source_delay")]
    pub per_source_delay_seconds: u64,
    #[serde(default)]
    pub sources: Vec<ScheduleSource>,
}

fn default_refresh_cron() -> String {
    "0 */6 * * *".to_string()
}

fn default_per_source_delay() -> u64 {
    5
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            refresh_cron: default_refresh_cron(),
            per_source_delay_seconds: default_per_source_delay(),
            sources: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ScheduleSource {
    pub url: String,
    #[serde(default = "default_source_limit")]
    pub limit: usize,
}

fn default_source_limit() -> usize {
    50
}

#[derive(Debug, Clone)]
pub enum BindAddress {
    Url(String),
    IpPort { ip: IpAddr, port: u16 },
}

impl<'de> Deserialize<'de> for BindAddress {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;

        if s.contains("://") {
            Url::parse(&s).map_err(serde::de::Error::custom)?;
            return Ok(BindAddress::Url(s.trim_end_matches('/').to_string()));
        }

        let (ip_str, port_str) = s.rsplit_once(':').ok_or_else(|| {
            serde::de::Error::custom(
                "address must be a URL with scheme (e.g. https://ytdlp.example.com) \
                 or an IP:port (e.g. 192.168.0.67:5000)",
            )
        })?;

        let ip = ip_str.parse().map_err(serde::de::Error::custom)?;
        let port = port_str.parse().map_err(serde::de::Error::custom)?;
        Ok(BindAddress::IpPort { ip, port })
    }
}

impl BindAddress {
    pub fn bind_addr(&self) -> SocketAddr {
        match self {
            BindAddress::Url(s) => {
                let port = Url::parse(s).ok().and_then(|u| u.port()).unwrap_or(8080);
                SocketAddr::from(([0, 0, 0, 0], port))
            }
            BindAddress::IpPort { ip, port } => SocketAddr::new(*ip, *port),
        }
    }

    pub fn base_url(&self) -> String {
        match self {
            BindAddress::Url(s) => s.clone(),
            BindAddress::IpPort { ip, port } => format!("http://{}:{}", ip, port),
        }
    }
}

impl SponsorBlockConfig {
    pub fn effective_categories(&self) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        parse_categories(&self.remove)
    }
}

fn parse_categories(spec: &str) -> Vec<String> {
    const ALL: &[&str] = &[
        "sponsor",
        "selfpromo",
        "interaction",
        "intro",
        "outro",
        "preview",
        "music_offtopic",
        "filler",
        "poi_highlight",
    ];
    const DEFAULT: &[&str] = &[
        "sponsor",
        "selfpromo",
        "interaction",
        "intro",
        "outro",
        "preview",
        "music_offtopic",
    ];
    match spec.trim() {
        "all" => ALL.iter().map(|s| s.to_string()).collect(),
        "default" => DEFAULT.iter().map(|s| s.to_string()).collect(),
        "" | "none" => Vec::new(),
        other => other
            .split(',')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect(),
    }
}

impl ScheduleConfig {
    pub fn is_enabled(&self) -> bool {
        !self.sources.is_empty() && !self.refresh_cron.trim().is_empty()
    }
}
