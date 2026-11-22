use scryer_youtube_library_service::{Service, media::YtDlp, router};
use std::{path::PathBuf, sync::Arc};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let executable = std::env::var_os("YTDLP_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("yt-dlp"));
    let height = std::env::var("YOUTUBE_MAX_HEIGHT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1080);
    let bind = std::env::var("YOUTUBE_LIBRARY_BIND").unwrap_or_else(|_| "0.0.0.0:8090".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(
        listener,
        router(Service::new(Arc::new(YtDlp::new(executable)), height)),
    )
    .await?;
    Ok(())
}
