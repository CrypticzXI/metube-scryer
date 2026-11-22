//! Fail-closed matching between a linked official upload source and a real
//! Scryer catalogue episode. Playlist positions are never episode numbers.
use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use crate::model::{SeriesSource, Video, VideoProbe};
use anyhow::{Result, anyhow, ensure};
use chrono::NaiveDate;
use scryer_plugin_sdk::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    App,
    catalog_metadata::{MetadataClient, MetadataConnection, ResolvedEpisode},
};

#[derive(Deserialize)]
#[serde(untagged)]
pub enum SearchInput {
    Envelope {
        request: PluginSearchRequest,
        metadata_connection: Option<MetadataConnection>,
    },
    Legacy(PluginSearchRequest),
}

static METADATA: OnceLock<Result<MetadataClient, String>> = OnceLock::new();
static CACHE: OnceLock<Mutex<VecDeque<Cached>>> = OnceLock::new();
struct Cached {
    key: [u8; 32],
    at: Instant,
    response: PluginSearchResponse,
}

pub async fn run(app: &App, input: SearchInput) -> Result<PluginSearchResponse> {
    tokio::time::timeout(Duration::from_secs(25), run_inner(app, input))
        .await
        .map_err(|_| anyhow!("YouTube episode search timed out; try again later"))?
}

async fn run_inner(app: &App, input: SearchInput) -> Result<PluginSearchResponse> {
    let (request, connection) = match input {
        SearchInput::Envelope {
            request,
            metadata_connection,
        } => (request, metadata_connection),
        SearchInput::Legacy(request) => (request, None),
    };
    if let (Some(season), Some(episode)) = (request.season, request.episode) {
        let sources = app.library.repo.series_sources().await?;
        ensure!(
            sources.len() <= 1000,
            "Too many linked series sources; remove unused links"
        );
        let ranked: Vec<_> = sources
            .iter()
            .filter(|source| source.enabled)
            .map(|source| (source, source_match_rank(source, &request)))
            .filter(|(_, rank)| *rank > 0)
            .collect();
        let best_rank = ranked.iter().map(|(_, rank)| *rank).max().unwrap_or(0);
        let matching: Vec<_> = ranked
            .into_iter()
            .filter(|(_, rank)| *rank == best_rank)
            .map(|(source, _)| source)
            .collect();
        // A source must be explicit and unique. No global YouTube searches may
        // become numbered releases without a trusted source mapping.
        if matching.len() != 1 {
            return Ok(PluginSearchResponse::default());
        }
        let source = matching[0];
        let connection = connection.as_ref().ok_or_else(|| anyhow!("Automatic episode matching requires Scryer metadata settings: configure Scryer URL and, if login is enabled, Scryer username and password on the YouTube indexer"))?;
        ensure!(
            !connection.url.trim().is_empty(),
            "Configure the Scryer metadata URL on the YouTube indexer"
        );
        ensure!(
            !source.channel_id.is_empty(),
            "Re-link the official source to capture its YouTube channel ID"
        );
        let key = cache_key(
            source,
            season,
            episode,
            app.library.default_height,
            connection,
            &request.ids,
        )?;
        if let Some(response) = cached(&key) {
            return Ok(response);
        }
        let client = METADATA
            .get_or_init(|| {
                MetadataClient::new()
                    .map_err(|_| "Could not initialize Scryer metadata connection".to_owned())
            })
            .as_ref()
            .map_err(|message| anyhow!("{message}"))?;
        let resolved = client
            .resolve(connection, source.scryer_title_id.as_deref(), &request)
            .await?;
        // A configured title UUID does not permit a cross-series external-ID
        // conflict. Check the resolved identity against the source and request.
        if !resolved_matches(source, &request, &resolved) {
            return Ok(PluginSearchResponse::default());
        }
        ensure!(
            !resolved.episode_title.trim().is_empty(),
            "Scryer episode title is missing; refresh series metadata in Scryer"
        );
        if !descriptive_title(
            &resolved.episode_title,
            &source.channel_name,
            &resolved.series_name,
        ) {
            return Ok(PluginSearchResponse::default());
        }
        let query = format!("{} {}", source.channel_name, resolved.episode_title);
        ensure!(query.len() <= 200, "Episode search text is too long");
        let listing =
            app.library.search(&query).await.map_err(|_| {
                anyhow!("YouTube episode search failed; check yt-dlp and try again")
            })?;
        let mut candidates: Vec<_> = listing
            .videos
            .into_iter()
            .filter(|video| {
                video
                    .channel_id
                    .as_deref()
                    .is_none_or(|id| id == source.channel_id)
                    && title_matches(
                        &resolved.episode_title,
                        &video.title,
                        &source.channel_name,
                        &resolved.series_name,
                    )
            })
            .collect();
        candidates.sort_by(|a, b| a.id.cmp(&b.id));
        candidates.dedup_by(|a, b| a.id == b.id);
        // More candidates than the bounded probe budget is ambiguous, too.
        if candidates.len() > 3 {
            return Ok(PluginSearchResponse::default());
        }
        let mut verified = Vec::new();
        for candidate in candidates {
            let canonical = crate::model::youtube_url(&candidate.url)
                .map_err(|_| anyhow!("YouTube search returned an invalid video URL"))?;
            let probe = app
                .library
                .media
                .probe(&canonical, app.library.default_height)
                .await
                .map_err(|_| {
                    anyhow!("Could not verify YouTube video metadata; update yt-dlp and try again")
                })?;
            if probe.video.id == candidate.id && verified_video(source, &resolved, &probe) {
                verified.push(probe);
            }
        }
        if verified.len() != 1 {
            return Ok(PluginSearchResponse::default());
        }
        let response = PluginSearchResponse {
            results: vec![episode_result(&resolved, verified.remove(0))],
            ..Default::default()
        };
        cache_success(key, &response);
        return Ok(response);
    }
    if request.season.is_some()
        || request.episode.is_some()
        || request.absolute_episode.is_some()
        || request.context.as_ref().is_some_and(|context| {
            matches!(
                context.search_origin,
                PluginSearchOrigin::Automatic | PluginSearchOrigin::Rss
            )
        })
    {
        return Ok(PluginSearchResponse::default());
    }
    if request.query.trim().is_empty() {
        return Ok(PluginSearchResponse::default());
    }
    let inspection = if request.query.starts_with("https://") {
        app.library.inspect(&request.query).await?
    } else {
        app.library.search(&request.query).await?
    };
    Ok(PluginSearchResponse {
        results: inspection
            .videos
            .into_iter()
            .take(request.limit.clamp(1, 20))
            .map(interactive_result)
            .collect(),
        ..Default::default()
    })
}

fn known_ids<'a>(ids: impl Iterator<Item = (&'a String, &'a String)>) -> HashMap<String, String> {
    ids.filter_map(|(key, value)| {
        let key = normalize_external_id_key(key);
        matches!(key.as_str(), "tvdb_id" | "tmdb_id" | "imdb_id").then(|| (key, value.clone()))
    })
    .collect()
}

pub(crate) fn source_match_rank(source: &SeriesSource, request: &PluginSearchRequest) -> u8 {
    let ids = known_ids(request.ids.iter());
    let source_ids = known_ids(source.external_ids.iter());
    if ids
        .iter()
        .any(|(key, value)| source_ids.get(key).is_some_and(|other| other != value))
    {
        return 0;
    }
    if ids
        .iter()
        .any(|(key, value)| source_ids.get(key) == Some(value))
    {
        return 2;
    }
    if normalized(&source.series_name) == normalized(&series_query(&request.query)) {
        1
    } else {
        0
    }
}

fn resolved_matches(
    source: &SeriesSource,
    request: &PluginSearchRequest,
    resolved: &ResolvedEpisode,
) -> bool {
    let actual = known_ids(resolved.external_ids.iter());
    let wanted = known_ids(request.ids.iter());
    let linked = known_ids(source.external_ids.iter());
    if wanted
        .iter()
        .chain(linked.iter())
        .any(|(key, value)| actual.get(key) != Some(value))
    {
        return false;
    }
    source
        .scryer_title_id
        .as_ref()
        .is_none_or(|id| id == &resolved.title_id)
        && (source.scryer_title_id.is_some()
            || !linked.is_empty()
            || normalized(&source.series_name) == normalized(&resolved.series_name))
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}
fn normalized(text: &str) -> String {
    words(text).join(" ")
}
fn numeric_tokens(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .collect()
}
fn descriptive_title(title: &str, channel: &str, series: &str) -> bool {
    let mut tokens = words(title);
    for prefix in [words(channel), words(series)] {
        if !prefix.is_empty() && tokens.starts_with(&prefix) {
            tokens.drain(..prefix.len());
            break;
        }
    }
    tokens.iter().any(|word| {
        word.chars().any(char::is_alphabetic)
            && word.len() >= 3
            && ![
                "episode",
                "ep",
                "season",
                "part",
                "chapter",
                "special",
                "tba",
                "tbd",
                "to",
                "be",
                "announced",
                "determined",
                "unknown",
                "untitled",
                "title",
                "pending",
            ]
            .contains(&word.as_str())
    })
}
fn title_matches(expected: &str, actual: &str, channel: &str, series: &str) -> bool {
    if numeric_tokens(expected) != numeric_tokens(actual) {
        return false;
    }
    let trim_prefix = |text: &str| {
        let mut tokens = words(text);
        for prefix in [words(channel), words(series)] {
            if !prefix.is_empty() && tokens.len() > prefix.len() && tokens.starts_with(&prefix) {
                tokens.drain(..prefix.len());
                break;
            }
        }
        tokens
    };
    let expected = trim_prefix(expected);
    !expected.is_empty() && expected == trim_prefix(actual)
}

fn series_query(query: &str) -> String {
    let lower = query.to_ascii_lowercase();
    for (index, _) in lower.match_indices('s') {
        if index > 0 && lower.as_bytes()[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let tail = &lower.as_bytes()[index + 1..];
        let season = tail.iter().take_while(|b| b.is_ascii_digit()).count();
        if season == 0 || tail.get(season) != Some(&b'e') {
            continue;
        }
        let episode = tail[season + 1..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if episode > 0
            && tail[season + 1 + episode..]
                .iter()
                .all(|b| b.is_ascii_whitespace() || b".-_".contains(b))
        {
            return query[..index]
                .trim_end_matches(|c: char| c.is_whitespace() || ".-_".contains(c))
                .to_owned();
        }
    }
    query.trim().to_owned()
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(value, "%Y%m%d"))
        .ok()
}
fn verified_video(source: &SeriesSource, episode: &ResolvedEpisode, probe: &VideoProbe) -> bool {
    let video = &probe.video;
    if video.channel_id.as_deref() != Some(source.channel_id.as_str())
        || probe.height == 0
        || !title_matches(
            &episode.episode_title,
            &video.title,
            &source.channel_name,
            &episode.series_name,
        )
    {
        return false;
    }
    if crate::model::youtube_url(&video.url).ok().as_deref()
        != Some(format!("https://www.youtube.com/watch?v={}", video.id).as_str())
    {
        return false;
    }
    if let Some(air_date) = episode.air_date.as_deref() {
        let Some((air, upload)) =
            parse_date(air_date).zip(video.upload_date.as_deref().and_then(parse_date))
        else {
            return false;
        };
        if (air - upload).num_days().abs() > 1 {
            return false;
        }
    }
    if let Some(expected) = episode.duration_seconds.filter(|s| *s > 0) {
        let Some(actual) = video.duration.filter(|d| d.is_finite() && *d > 0.0) else {
            return false;
        };
        if actual < expected as f64 * 0.70 || actual > expected as f64 * 1.35 {
            return false;
        }
    }
    true
}

fn codec_label(codec: Option<&str>) -> Option<&'static str> {
    match codec? {
        c if c.starts_with("avc") || c == "h264" => Some("H.264"),
        c if c.starts_with("hev") || c.starts_with("hvc") || c == "hevc" || c == "h265" => {
            Some("H.265")
        }
        c if c.starts_with("av01") || c == "av1" => Some("AV1"),
        c if c.starts_with("vp09") || c == "vp9" => Some("VP9"),
        _ => None,
    }
}
fn resource(video: &Video, probe: Option<&VideoProbe>) -> PluginHttpResource {
    PluginHttpResource {
        transport: HttpTransport::ProviderPage,
        role: HttpResourceRole::Muxed,
        url: video.url.clone(),
        video: probe.map(|p| PluginHttpVideoTrack {
            height: Some(p.height),
            codec: p.video_codec.clone(),
            ..Default::default()
        }),
        expires_at: None,
        headers: vec![],
        auth_ref: None,
        size_bytes: None,
        size_estimate: None,
        validators: PluginHttpValidators::default(),
        container: None,
        audio: None,
        subtitle: None,
        separate_tracks: false,
        range_support: HttpRangeSupport::Unknown,
    }
}
fn interactive_result(video: Video) -> PluginSearchResult {
    PluginSearchResult {
        title: format!("{} - {}", video.channel, video.title),
        link: Some(video.url.clone()),
        download_url: Some(video.url.clone()),
        info_url: Some(video.url.clone()),
        guid: Some(format!("youtube:{}", video.id)),
        source_kind: Some(IndexerSourceKind::Http),
        protocol: Some(IndexerProtocol::Http),
        http: Some(PluginHttpRelease {
            provider: "youtube".into(),
            item_id: video.id.clone(),
            page_url: video.url.clone(),
            channel: Some(PluginHttpChannel {
                id: video.channel_id.clone(),
                name: video.channel.clone(),
            }),
            duration_seconds: video
                .duration
                .filter(|d| d.is_finite() && *d > 0.0)
                .map(|d| d as u64),
            resources: vec![resource(&video, None)],
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn episode_result(episode: &ResolvedEpisode, probe: VideoProbe) -> PluginSearchResult {
    let mut result = interactive_result(probe.video.clone());
    let codec = codec_label(probe.video_codec.as_deref())
        .map(|c| format!(" {c}"))
        .unwrap_or_default();
    result.title = format!(
        "{} S{:02}E{:02} {}p WEB-DL{}",
        episode.series_name, episode.season, episode.episode, probe.height, codec
    );
    result.size_bytes = probe.size_bytes.filter(|size| *size > 0);
    result.external_ids = episode.external_ids.clone();
    result
        .provider_extra
        .insert("raw_title".into(), serde_json::json!(probe.video.title));
    result.provider_extra.insert(
        "verified_episode_title".into(),
        serde_json::json!(episode.episode_title),
    );
    if let Some(http) = result.http.as_mut() {
        http.availability = HttpAvailability::Available;
        http.resources = vec![resource(&probe.video, Some(&probe))];
    }
    result
}

fn cache_key(
    source: &SeriesSource,
    season: u32,
    episode: u32,
    height: u32,
    connection: &MetadataConnection,
    request_ids: &HashMap<String, String>,
) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    // Credential changes invalidate cache identity; secrets themselves never
    // enter stored keys, responses, errors or logs.
    for bytes in [
        serde_json::to_vec(source)?,
        serde_json::to_vec(connection)?,
        serde_json::to_vec(&(season, episode, height))?,
        serde_json::to_vec(
            &request_ids
                .iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
        )?,
    ] {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    Ok(hash.finalize().into())
}
fn cached(key: &[u8; 32]) -> Option<PluginSearchResponse> {
    let mut entries = CACHE
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .ok()?;
    entries.retain(|entry| entry.at.elapsed() < Duration::from_secs(300));
    entries
        .iter()
        .find(|entry| &entry.key == key)
        .map(|entry| entry.response.clone())
}
fn cache_success(key: [u8; 32], response: &PluginSearchResponse) {
    if let Ok(mut entries) = CACHE.get_or_init(|| Mutex::new(VecDeque::new())).lock() {
        entries.retain(|entry| entry.key != key && entry.at.elapsed() < Duration::from_secs(300));
        while entries.len() >= 256 {
            entries.pop_front();
        }
        entries.push_back(Cached {
            key,
            at: Instant::now(),
            response: response.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> SeriesSource {
        SeriesSource {
            id: "source".into(),
            series_name: "Sidemen Sundays".into(),
            scryer_title_id: None,
            external_ids: std::collections::BTreeMap::from([("tvdb".into(), "388373".into())]),
            channel_id: "UCofficial".into(),
            channel_name: "Sidemen".into(),
            channel_url: "https://www.youtube.com/@Sidemen".into(),
            enabled: true,
        }
    }
    fn episode() -> ResolvedEpisode {
        ResolvedEpisode {
            title_id: "title".into(),
            series_name: "Sidemen Sundays".into(),
            external_ids: HashMap::from([("tvdb".into(), "388373".into())]),
            season: 2,
            episode: 4,
            episode_title: "Sidemen 20 vs 1".into(),
            air_date: Some("2026-11-20".into()),
            duration_seconds: Some(3600),
        }
    }
    fn probe() -> VideoProbe {
        VideoProbe {
            video: Video {
                id: "abcdefghijk".into(),
                url: "https://www.youtube.com/watch?v=abcdefghijk".into(),
                title: "SIDEMEN 20 VS 1".into(),
                channel: "Sidemen".into(),
                channel_id: Some("UCofficial".into()),
                thumbnail: None,
                duration: Some(3600.0),
                upload_date: Some("20261120".into()),
            },
            height: 1080,
            video_codec: Some("avc1".into()),
            container: Some("mp4".into()),
            size_bytes: None,
        }
    }
    #[test]
    fn exact_title_matching_preserves_every_number() {
        assert!(title_matches(
            "Sidemen 20 vs 1",
            "20 VS 1",
            "Sidemen",
            "Sidemen Sundays"
        ));
        assert!(!title_matches(
            "Sidemen 20 vs 1",
            "SIDEMEN 20 VS 2",
            "Sidemen",
            "Sidemen Sundays"
        ));
        assert!(!title_matches(
            "Sidemen 20 vs 1",
            "SIDEMEN 20 VS 1 highlights",
            "Sidemen",
            "Sidemen Sundays"
        ));
        assert!(!title_matches("", "anything", "Sidemen", "Sidemen Sundays"));
        for generic in [
            "Episode 47",
            "TBA",
            "SIDEMEN TBA",
            "Untitled",
            "To Be Announced",
        ] {
            assert!(!descriptive_title(generic, "Sidemen", "Sidemen Sundays"));
        }
        assert!(descriptive_title(
            "SIDEMEN PUB QUIZ",
            "Sidemen",
            "Sidemen Sundays"
        ));
    }
    #[test]
    fn rejects_wrong_channel_date_duration_and_missing_evidence() {
        let (source, episode) = (source(), episode());
        assert!(verified_video(&source, &episode, &probe()));
        let mut p = probe();
        p.video.channel_id = Some("UCimposter".into());
        assert!(!verified_video(&source, &episode, &p));
        let mut p = probe();
        p.video.upload_date = Some("20261122".into());
        assert!(!verified_video(&source, &episode, &p));
        let mut p = probe();
        p.video.duration = Some(120.0);
        assert!(!verified_video(&source, &episode, &p));
        let mut p = probe();
        p.video.upload_date = None;
        assert!(!verified_video(&source, &episode, &p));
    }
    #[test]
    fn source_identity_never_falls_back_from_conflicting_ids() {
        let mut request = PluginSearchRequest {
            query: "Sidemen Sundays S02E04".into(),
            ..Default::default()
        };
        assert!((source_match_rank(&source(), &request) > 0));
        request.ids.insert("tvdb_id".into(), "999".into());
        assert!(!(source_match_rank(&source(), &request) > 0));
        request.ids.insert("tvdb_id".into(), "388373".into());
        assert!((source_match_rank(&source(), &request) > 0));
        request.ids.insert("tmdb".into(), "incorrect".into());
        assert!(!resolved_matches(&source(), &request, &episode()));
        assert_eq!(series_query("Series S02E04 extra"), "Series S02E04 extra");
        let mut uuid_only = source();
        uuid_only.external_ids.clear();
        uuid_only.scryer_title_id = Some("title".into());
        assert_eq!(source_match_rank(&uuid_only, &request), 1);
        assert!(!resolved_matches(&uuid_only, &request, &episode()));
        request.ids.remove("tmdb");
        assert!(resolved_matches(&uuid_only, &request, &episode()));
        assert_eq!(source_match_rank(&source(), &request), 2);
    }
    #[test]
    fn canonical_release_uses_resolved_numbering_and_real_quality() {
        let result = episode_result(&episode(), probe());
        assert_eq!(result.title, "Sidemen Sundays S02E04 1080p WEB-DL H.264");
        let http = result.http.unwrap();
        assert_eq!(http.channel.unwrap().id.as_deref(), Some("UCofficial"));
        assert_eq!(http.resources[0].video.as_ref().unwrap().height, Some(1080));
        assert_eq!(result.size_bytes, None);
    }
    #[test]
    fn accepts_both_wire_shapes_without_exposing_credentials() {
        assert!(matches!(
            serde_json::from_value::<SearchInput>(serde_json::json!({"query":"test"})).unwrap(),
            SearchInput::Legacy(_)
        ));
        assert!(matches!(serde_json::from_value::<SearchInput>(serde_json::json!({"request":{"query":"test"},"metadata_connection":{"url":"http://scryer","username":null,"password":null}})).unwrap(),SearchInput::Envelope{..}));
        let c = MetadataConnection {
            url: "http://scryer".into(),
            username: None,
            password: None,
        };
        let a = cache_key(&source(), 2, 4, 1080, &c, &HashMap::new()).unwrap();
        let mut changed = c;
        changed.password = Some("secret".into());
        assert_ne!(
            a,
            cache_key(&source(), 2, 4, 1080, &changed, &HashMap::new()).unwrap()
        );
    }
}
