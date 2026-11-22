#![cfg(feature = "test-fixtures")]
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::Request,
    routing::{get, post},
};
use scryer_plugin_sdk::{PluginResult, PluginSearchResponse};
use scryer_youtube_library_service::{Service, media::YtDlp, router};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tower::ServiceExt;

#[tokio::test]
async fn endpoint_resolves_authoritative_episode_through_real_fixture_process() {
    async fn graphql(headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(headers["x-scryer-web-client"], "fixture-proof");
        assert_eq!(headers["cookie"], "fixture=fixture-cookie");
        let query = body["query"].as_str().unwrap();
        if query.contains("collectionById") {
            assert_eq!(body["variables"]["id"], "season-2");
            Json(
                json!({"data":{"collectionById":{"episodes":[{"seasonNumber":"2","episodeNumber":"3","title":"The Exact Episode","airDate":"2026-11-20","durationSeconds":120}]}}}),
            )
        } else {
            assert!(!query.contains("episodes"));
            Json(
                json!({"data":{"title":{"id":"title-fixture","name":"Fixture Series","externalIds":[{"source":"tvdb","value":"123"}],"collections":[{"id":"season-2","collectionIndex":"2"}]}}}),
            )
        }
    }
    let metadata = Router::new().route("/graphql", post(graphql)).route(
        "/authless-client",
        get(|| async {
            (
                [("set-cookie", "fixture=fixture-cookie; Path=/; HttpOnly")],
                Json(
                    json!({"proof":"fixture-proof","expiresAt":chrono::Utc::now().timestamp()+600}),
                ),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, metadata).await.unwrap();
    });
    let provider = YtDlp::new(PathBuf::from(env!("CARGO_BIN_EXE_youtube-library-fixture")));
    let app = router(Service::new(Arc::new(provider), 1080));
    let input = json!({"request":{"query":"Fixture Series S02E03","season":2,"episode":3,"ids":{"tvdb_id":"123"}},
        "metadata_connection":{"url":base},"sources":[{"series_name":"Fixture Series","scryer_title_id":"title-fixture","tvdb_id":"123","channel_url":"https://www.youtube.com/@Official"}]});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/scryer/search")
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let result: PluginResult<PluginSearchResponse> =
        serde_json::from_slice(&to_bytes(response.into_body(), 128 * 1024).await.unwrap()).unwrap();
    let PluginResult::Ok(response) = result else {
        panic!("fixture episode lookup failed")
    };
    assert_eq!(response.results.len(), 1);
    let result = &response.results[0];
    assert_eq!(result.title, "Fixture Series S02E03 720p WEB-DL H.264");
    let http = result.http.as_ref().unwrap();
    assert_eq!(http.provider, "youtube");
    assert_eq!(
        http.channel.as_ref().unwrap().id.as_deref(),
        Some("UCaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(
        result.download_url.as_deref(),
        Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
    );
    server.abort();
}
