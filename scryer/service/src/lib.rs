//! Read-only YouTube episode indexer. Acquisition belongs to the MeTube download-client plugin.
pub mod catalog_metadata;
mod episode_search;
pub mod media;
pub mod model;

use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    routing::{get, post},
};
use catalog_metadata::MetadataConnection;
use model::{Inspection, SeriesSource, VideoProbe};
use scryer_plugin_sdk::{
    PluginError, PluginErrorCode, PluginResult, PluginSearchRequest, PluginSearchResponse,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[async_trait]
pub trait MediaProvider: Send + Sync {
    async fn health(&self) -> Result<String>;
    async fn inspect(&self, url: &str, limit: u32) -> Result<Inspection>;
    async fn search(&self, query: &str, limit: u32) -> Result<Inspection>;
    async fn probe(&self, url: &str, max_height: u32) -> Result<VideoProbe>;
}

pub struct Sources(Vec<SeriesSource>);
impl Sources {
    pub async fn series_sources(&self) -> Result<Vec<SeriesSource>> {
        Ok(self.0.clone())
    }
}

pub struct Library {
    pub repo: Sources,
    pub media: Arc<dyn MediaProvider>,
    pub default_height: u32,
}
impl Library {
    pub async fn search(&self, query: &str) -> Result<Inspection> {
        self.media.search(query, 20).await
    }
    pub async fn inspect(&self, url: &str) -> Result<Inspection> {
        self.media.inspect(url, 20).await
    }
}
pub struct App {
    pub library: Library,
}

// Source configuration is forwarded on each request; it is never persisted here.
#[derive(Clone, Deserialize)]
pub struct SourceConfig {
    pub series_name: String,
    pub scryer_title_id: Option<String>,
    pub tvdb_id: Option<String>,
    pub channel_url: String,
}

#[derive(Deserialize)]
pub struct SearchEnvelope {
    pub request: PluginSearchRequest,
    pub metadata_connection: Option<MetadataConnection>,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
}

struct ChannelCache {
    url: String,
    at: Instant,
    channel_id: String,
    channel_name: String,
}
#[derive(Clone)]
pub struct Service {
    media: Arc<dyn MediaProvider>,
    height: u32,
    channels: Arc<Mutex<VecDeque<ChannelCache>>>,
}

impl Service {
    pub fn new(media: Arc<dyn MediaProvider>, height: u32) -> Self {
        Self {
            media,
            height: height.clamp(144, 4320),
            channels: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    async fn source(&self, mut source: SeriesSource) -> Result<SeriesSource> {
        {
            let mut entries = self
                .channels
                .lock()
                .map_err(|_| anyhow!("channel cache unavailable"))?;
            entries.retain(|entry| entry.at.elapsed() < Duration::from_secs(300));
            if let Some(entry) = entries.iter().find(|entry| entry.url == source.channel_url) {
                source.channel_id = entry.channel_id.clone();
                source.channel_name = entry.channel_name.clone();
                return Ok(source);
            }
        }
        let inspection = self.media.inspect(&source.channel_url, 1).await?;
        let channel_id = inspection
            .channel_id
            .filter(|id| {
                id.len() == 24
                    && id.starts_with("UC")
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            })
            .ok_or_else(|| anyhow!("Official source has no verified YouTube channel ID"))?;
        let channel_name = inspection
            .channel_name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                inspection
                    .videos
                    .first()
                    .map(|v| v.channel.clone())
                    .filter(|name| name != "Unknown channel")
            })
            .ok_or_else(|| anyhow!("Official source has no YouTube channel name"))?;
        source.channel_id = channel_id.clone();
        source.channel_name = channel_name.clone();
        let mut entries = self
            .channels
            .lock()
            .map_err(|_| anyhow!("channel cache unavailable"))?;
        entries.retain(|entry| {
            entry.url != source.channel_url && entry.at.elapsed() < Duration::from_secs(300)
        });
        while entries.len() >= 100 {
            entries.pop_front();
        }
        entries.push_back(ChannelCache {
            url: source.channel_url.clone(),
            at: Instant::now(),
            channel_id,
            channel_name,
        });
        Ok(source)
    }

    async fn search(&self, input: SearchEnvelope) -> Result<PluginSearchResponse> {
        ensure!(
            input.sources.len() <= 100,
            "At most 100 official sources may be configured"
        );
        let mut sources = Vec::new();
        // Interactive searches use provider pages and do not resolve episode numbers.
        if input.request.season.is_some() && input.request.episode.is_some() {
            let mut ranked = Vec::new();
            for (index, config) in input.sources.into_iter().enumerate() {
                ensure!(
                    !config.series_name.trim().is_empty() && config.series_name.len() <= 200,
                    "Official source series name is invalid"
                );
                ensure!(
                    config
                        .scryer_title_id
                        .as_ref()
                        .is_none_or(|id| !id.trim().is_empty() && id.len() <= 128),
                    "Scryer title ID is invalid"
                );
                let url = model::youtube_url(&config.channel_url)?;
                ensure!(
                    !url.contains("/watch?v="),
                    "Official source must be a YouTube channel or playlist"
                );
                let mut ids = BTreeMap::new();
                if let Some(id) = config.tvdb_id {
                    ensure!(
                        id.parse::<u64>().is_ok_and(|n| n > 0),
                        "Official source TVDB ID is invalid"
                    );
                    ids.insert("tvdb".into(), id);
                }
                let source = SeriesSource {
                    id: format!("{index}:{url}"),
                    series_name: config.series_name,
                    scryer_title_id: config.scryer_title_id,
                    external_ids: ids,
                    channel_id: String::new(),
                    channel_name: String::new(),
                    channel_url: url,
                    enabled: true,
                };
                let rank = episode_search::source_match_rank(&source, &input.request);
                if rank > 0 {
                    ranked.push((rank, source));
                }
            }
            let best = ranked.iter().map(|(rank, _)| *rank).max().unwrap_or(0);
            let mut matching = ranked.into_iter().filter(|(rank, _)| *rank == best);
            if let Some((_, source)) = matching.next() {
                if matching.next().is_some() {
                    return Ok(PluginSearchResponse::default());
                }
                sources.push(self.source(source).await?);
            }
        }
        let app = App {
            library: Library {
                repo: Sources(sources),
                media: self.media.clone(),
                default_height: self.height,
            },
        };
        episode_search::run(
            &app,
            episode_search::SearchInput::Envelope {
                request: input.request,
                metadata_connection: input.metadata_connection,
            },
        )
        .await
    }
}

pub fn router(service: Service) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/status", get(status))
        .route("/api/scryer/search", post(search))
        .layer(DefaultBodyLimit::max(128 * 1024))
        .with_state(service)
}

async fn status(State(service): State<Service>) -> Json<serde_json::Value> {
    let health = tokio::time::timeout(Duration::from_secs(12), service.media.health()).await;
    let yt_dlp = match health {
        Ok(Ok(version)) => Some(version),
        _ => None,
    };
    Json(json!({"version":env!("CARGO_PKG_VERSION"),"yt_dlp":yt_dlp}))
}

async fn search(
    State(service): State<Service>,
    Json(input): Json<SearchEnvelope>,
) -> Json<PluginResult<PluginSearchResponse>> {
    let result = tokio::time::timeout(Duration::from_secs(45), service.search(input))
        .await
        .unwrap_or_else(|_| Err(anyhow!("YouTube episode search timed out; try again later")));
    Json(match result {
        Ok(response) => PluginResult::Ok(response),
        Err(error) => PluginResult::Err(PluginError {
            code: PluginErrorCode::UpstreamUnavailable,
            public_message: error.to_string(),
            debug_message: None,
            retry_after_seconds: None,
            details: None,
        }),
    })
}

#[cfg(test)]
mod tests;
