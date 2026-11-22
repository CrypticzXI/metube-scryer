use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Video {
    pub id: String,
    pub url: String,
    pub title: String,
    pub channel: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    pub thumbnail: Option<String>,
    pub duration: Option<f64>,
    pub upload_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inspection {
    #[serde(default)]
    pub channel_name: Option<String>,
    pub title: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    pub url: String,
    pub is_collection: bool,
    pub videos: Vec<Video>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeriesSource {
    pub id: String,
    pub series_name: String,
    pub scryer_title_id: Option<String>,
    pub external_ids: std::collections::BTreeMap<String, String>,
    pub channel_id: String,
    pub channel_name: String,
    pub channel_url: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoProbe {
    pub video: Video,
    pub height: u32,
    pub video_codec: Option<String>,
    pub container: Option<String>,
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlError {
    InvalidUrl,
    UnsupportedHost,
    InvalidVideoId,
    InvalidPlaylist,
    InvalidChannel,
    UnsupportedUrl,
}
impl UrlError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidUrl => "YOUTUBE_URL_INVALID",
            Self::UnsupportedHost => "YOUTUBE_HOST_UNSUPPORTED",
            Self::InvalidVideoId => "YOUTUBE_VIDEO_ID_INVALID",
            Self::InvalidPlaylist => "YOUTUBE_PLAYLIST_INVALID",
            Self::InvalidChannel => "YOUTUBE_CHANNEL_INVALID",
            Self::UnsupportedUrl => "YOUTUBE_URL_UNSUPPORTED",
        }
    }
}
impl std::fmt::Display for UrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidUrl => {
                "Enter a valid HTTPS YouTube link without a username, password, or custom port."
            }
            Self::UnsupportedHost => "Use a YouTube link from youtube.com or youtu.be.",
            Self::InvalidVideoId => "This link does not contain a valid YouTube video ID.",
            Self::InvalidPlaylist => "This link does not contain a valid YouTube playlist ID.",
            Self::InvalidChannel => {
                "Enter a valid YouTube channel link, such as https://youtube.com/@BetaSquad."
            }
            Self::UnsupportedUrl => "Paste a video, playlist, or channel link from YouTube.",
        })
    }
}
impl std::error::Error for UrlError {}
// Accept provider pages only. Never pass arbitrary URLs or command options to yt-dlp.
pub fn youtube_url(input: &str) -> Result<String, UrlError> {
    let url = Url::parse(input.trim()).map_err(|_| UrlError::InvalidUrl)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(UrlError::InvalidUrl);
    }
    let host = url.host_str().ok_or(UrlError::InvalidUrl)?;
    if !matches!(
        host,
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be"
    ) {
        return Err(UrlError::UnsupportedHost);
    }
    let p = url.path();
    if host == "youtu.be" {
        let id = p.trim_start_matches('/');
        if !valid_video_id(id) {
            return Err(UrlError::InvalidVideoId);
        }
        return Ok(format!("https://www.youtube.com/watch?v={id}"));
    }
    if p == "/watch" {
        let id = url
            .query_pairs()
            .find(|(k, _)| k == "v")
            .map(|(_, v)| v.into_owned())
            .ok_or(UrlError::InvalidVideoId)?;
        if !valid_video_id(&id) {
            return Err(UrlError::InvalidVideoId);
        }
        return Ok(format!("https://www.youtube.com/watch?v={id}"));
    }
    if let Some(id) = p
        .strip_prefix("/shorts/")
        .or_else(|| p.strip_prefix("/live/"))
    {
        if !valid_video_id(id) {
            return Err(UrlError::InvalidVideoId);
        }
        return Ok(format!("https://www.youtube.com/watch?v={id}"));
    }
    if p == "/playlist" {
        let id = url
            .query_pairs()
            .find(|(k, _)| k == "list")
            .map(|(_, v)| v.into_owned())
            .ok_or(UrlError::InvalidPlaylist)?;
        if id.is_empty()
            || id.len() > 150
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            return Err(UrlError::InvalidPlaylist);
        }
        return Ok(format!("https://www.youtube.com/playlist?list={id}"));
    }
    if p.starts_with("/@")
        || p.starts_with("/channel/")
        || p.starts_with("/c/")
        || p.starts_with("/user/")
    {
        let base = p
            .trim_end_matches('/')
            .trim_end_matches("/videos")
            .trim_end_matches("/streams")
            .trim_end_matches("/shorts");
        if base.len() < 3 || base.len() > 200 || base.contains("..") || base.contains('%') {
            return Err(UrlError::InvalidChannel);
        }
        return Ok(format!("https://www.youtube.com{base}/videos"));
    }
    Err(UrlError::UnsupportedUrl)
}
pub fn valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
