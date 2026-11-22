use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

struct FixtureProvider {
    inspections: AtomicUsize,
}
#[async_trait]
impl MediaProvider for FixtureProvider {
    async fn health(&self) -> Result<String> {
        Ok("fixture-1.0".into())
    }
    async fn inspect(&self, url: &str, _limit: u32) -> Result<Inspection> {
        self.inspections.fetch_add(1, Ordering::SeqCst);
        Ok(Inspection {
            title: "Official uploads".into(),
            channel_name: Some("Official".into()),
            channel_id: Some("UCaaaaaaaaaaaaaaaaaaaaaa".into()),
            url: url.into(),
            is_collection: true,
            videos: vec![],
            truncated: false,
        })
    }
    async fn search(&self, _query: &str, _limit: u32) -> Result<Inspection> {
        anyhow::bail!("unexpected search")
    }
    async fn probe(&self, _url: &str, _height: u32) -> Result<VideoProbe> {
        anyhow::bail!("unexpected probe")
    }
}

#[tokio::test]
async fn status_endpoint_exposes_provider_health() {
    let service = Service::new(
        Arc::new(FixtureProvider {
            inspections: AtomicUsize::new(0),
        }),
        1080,
    );
    let response = router(service)
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["yt_dlp"], "fixture-1.0");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn verifies_official_channel_identity_and_caches_only_channel_metadata() {
    let provider = Arc::new(FixtureProvider {
        inspections: AtomicUsize::new(0),
    });
    let service = Service::new(provider.clone(), 1080);
    let source = SeriesSource {
        id: "source".into(),
        series_name: "Series".into(),
        scryer_title_id: None,
        external_ids: Default::default(),
        channel_id: String::new(),
        channel_name: String::new(),
        channel_url: "https://www.youtube.com/@Official/videos".into(),
        enabled: true,
    };
    for _ in 0..2 {
        let resolved = service.source(source.clone()).await.unwrap();
        assert_eq!(resolved.channel_id, "UCaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(resolved.channel_name, "Official");
    }
    assert_eq!(provider.inspections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn source_limit_is_enforced_before_provider_calls() {
    let provider = Arc::new(FixtureProvider {
        inspections: AtomicUsize::new(0),
    });
    let service = Service::new(provider.clone(), 1080);
    let input = SearchEnvelope {
        request: PluginSearchRequest::default(),
        metadata_connection: None,
        sources: vec![
            SourceConfig {
                series_name: "Series".into(),
                scryer_title_id: None,
                tvdb_id: None,
                channel_url: "https://www.youtube.com/@Official".into()
            };
            101
        ],
    };
    assert!(service.search(input).await.is_err());
    assert_eq!(provider.inspections.load(Ordering::SeqCst), 0);
}

#[test]
fn source_list_defaults_empty_and_metadata_is_optional() {
    let input: SearchEnvelope =
        serde_json::from_value(json!({"request":{"query":"test"}})).unwrap();
    assert!(input.sources.is_empty());
    assert!(input.metadata_connection.is_none());
}

#[test]
fn youtube_targets_are_canonical_provider_pages_only() {
    assert_eq!(
        model::youtube_url("https://youtu.be/dQw4w9WgXcQ?t=12").unwrap(),
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
    );
    for url in [
        "--exec=whoami",
        "https://youtube.com.evil.test/watch?v=dQw4w9WgXcQ",
        "https://user:password@youtube.com/watch?v=dQw4w9WgXcQ",
        "http://youtube.com/@Official",
    ] {
        assert!(model::youtube_url(url).is_err());
    }
}
