use crate::{MediaProvider, model::*};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
pub struct YtDlp {
    pub executable: PathBuf,
    gate: std::sync::Arc<tokio::sync::Semaphore>,
}
impl YtDlp {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            gate: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
    fn command(&self) -> Command {
        let mut c = Command::new(&self.executable);
        if let Ok(runtime) = std::env::var("YTDLP_JS_RUNTIME") {
            c.args(["--js-runtimes", &runtime]);
        }
        c.args([
            "--ignore-config",
            "--no-colors",
            "--no-cache-dir",
            "--socket-timeout",
            "20",
            "--retries",
            "3",
            "--extractor-retries",
            "2",
        ]);
        c.kill_on_drop(true);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.as_std_mut().creation_flags(0x08000000);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.as_std_mut().process_group(0);
        }
        c
    }
    async fn capture(&self, args: &[String], timeout: Duration) -> Result<Vec<u8>> {
        let _permit = self
            .gate
            .clone()
            .try_acquire_owned()
            .context("METADATA_BUSY: two lookups are already running; retry shortly")?;
        let mut child = self
            .command()
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("YTDLP_START_FAILED: install yt-dlp or configure YTDLP_BIN")?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let task = async move {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut stdout = stdout.take(16 * 1024 * 1024 + 1);
            let mut stderr = stderr.take(64 * 1024 + 1);
            tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))?;
            Ok::<_, std::io::Error>((out, err))
        };
        let (out, err) = match tokio::time::timeout(timeout, task).await {
            Ok(r) => r?,
            Err(_) => {
                terminate(&mut child).await;
                bail!("YTDLP_METADATA_TIMEOUT");
            }
        };
        if out.len() > 16 * 1024 * 1024 || err.len() > 64 * 1024 {
            terminate(&mut child).await;
            bail!("YTDLP_OUTPUT_LIMIT");
        }
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .context("YTDLP_EXIT_TIMEOUT")??;
        if !status.success() {
            bail!("YTDLP_METADATA_FAILED: update yt-dlp and retry");
        }
        Ok(out)
    }
    async fn metadata(&self, target: &str, limit: u32) -> Result<Inspection> {
        let limit = limit.clamp(1, 100);
        let args = vec![
            "--flat-playlist".into(),
            "--dump-single-json".into(),
            "--skip-download".into(),
            "--ignore-errors".into(),
            "--playlist-end".into(),
            (limit + 1).to_string(),
            "--".into(),
            target.into(),
        ];
        let out = self.capture(&args, Duration::from_secs(90)).await?;
        let data: Value = serde_json::from_slice(&out).context("YTDLP_METADATA_INVALID")?;
        parse_inspection(data, target, limit as usize)
    }
}
fn parse_video(v: &Value) -> Option<Video> {
    let id = v.get("id")?.as_str()?;
    if !valid_video_id(id) {
        return None;
    }
    let thumbnail = v
        .get("thumbnail")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")));
    Some(Video {
        id: id.into(),
        url: format!("https://www.youtube.com/watch?v={id}"),
        title: v.get("title").and_then(Value::as_str).unwrap_or(id).into(),
        channel: v
            .get("channel")
            .and_then(Value::as_str)
            .or_else(|| v.get("uploader").and_then(Value::as_str))
            .unwrap_or("Unknown channel")
            .into(),
        channel_id: v
            .get("channel_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        thumbnail,
        duration: v.get("duration").and_then(Value::as_f64),
        upload_date: v
            .get("upload_date")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}
fn parse_inspection(data: Value, url: &str, limit: usize) -> Result<Inspection> {
    let is_collection = data.get("entries").is_some();
    let mut videos = Vec::new();
    let mut truncated = false;
    if let Some(entries) = data.get("entries").and_then(Value::as_array) {
        truncated = entries.len() > limit;
        videos = entries.iter().take(limit).filter_map(parse_video).collect();
    } else if let Some(v) = parse_video(&data) {
        videos.push(v);
    }
    if videos.is_empty() {
        bail!("YOUTUBE_NO_PUBLIC_VIDEOS");
    }
    if url.starts_with("https://www.youtube.com/") && url.ends_with("/videos") {
        if let Some(channel_id) = data
            .get("channel_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            for video in &mut videos {
                if video.channel_id.is_none() {
                    video.channel_id = Some(channel_id.to_owned());
                }
            }
        }
        let channel = data
            .get("channel")
            .and_then(Value::as_str)
            .or_else(|| data.get("uploader").and_then(Value::as_str))
            .or_else(|| {
                data.get("title")
                    .and_then(Value::as_str)
                    .map(|s| s.trim_end_matches(" - Videos"))
            });
        if let Some(channel) = channel {
            for video in &mut videos {
                if video.channel == "Unknown channel" {
                    video.channel = channel.into();
                }
            }
        }
    }
    Ok(Inspection {
        channel_name: data
            .get("channel")
            .and_then(Value::as_str)
            .map(str::to_owned),
        channel_id: data
            .get("channel_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        title: data
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("YouTube")
            .into(),
        url: url.into(),
        is_collection,
        videos,
        truncated,
    })
}
fn selected_format(max_height: u32, audio: &str) -> String {
    format!("bv*[height<={max_height}]+{audio}/b[height<={max_height}]")
}

fn parse_probe(data: &Value, max_height: u32) -> Result<VideoProbe> {
    let video = parse_video(data).ok_or_else(|| anyhow::anyhow!("YTDLP_VIDEO_METADATA_INVALID"))?;
    // requested_downloads represents the selected download, not the list of all available formats.
    let selected = match data.get("requested_downloads").and_then(Value::as_array) {
        Some(downloads) if downloads.len() == 1 => &downloads[0],
        Some(_) => bail!("YTDLP_SELECTED_FORMAT_AMBIGUOUS"),
        None => data,
    };
    let streams: Vec<&Value> = match selected.get("requested_formats").and_then(Value::as_array) {
        Some(formats) => formats.iter().collect(),
        None => vec![selected],
    };
    let mut videos = streams.iter().filter(|stream| {
        stream
            .get("vcodec")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty() && s != "none")
    });
    let stream = videos
        .next()
        .ok_or_else(|| anyhow::anyhow!("YTDLP_SELECTED_VIDEO_MISSING"))?;
    if videos.next().is_some() {
        bail!("YTDLP_SELECTED_VIDEO_AMBIGUOUS");
    }
    let height = stream
        .get("height")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0 && *n <= max_height)
        .ok_or_else(|| anyhow::anyhow!("YTDLP_SELECTED_HEIGHT_UNKNOWN_OR_EXCEEDS_LIMIT"))?;
    let video_codec = stream
        .get("vcodec")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let container = if streams.len() > 1 {
        Some("mkv".to_owned())
    } else {
        selected
            .get("ext")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    // Pre-mux component sizes and provider estimates are not an exact output size.
    let size_bytes = if streams.len() == 1 {
        streams[0]
            .get("filesize")
            .and_then(Value::as_i64)
            .filter(|n| *n > 0)
    } else {
        None
    };
    Ok(VideoProbe {
        video,
        height,
        video_codec,
        container,
        size_bytes,
    })
}

#[async_trait]
impl MediaProvider for YtDlp {
    async fn probe(&self, url: &str, max_height: u32) -> Result<VideoProbe> {
        let url = youtube_url(url).map_err(anyhow::Error::new)?;
        if !url.contains("/watch?v=") {
            bail!("VIDEO_URL_REQUIRED");
        }
        if max_height == 0 {
            bail!("VIDEO_HEIGHT_INVALID");
        }
        let args = vec![
            "--no-playlist".into(),
            "--dump-single-json".into(),
            "--skip-download".into(),
            "--format".into(),
            selected_format(max_height, "ba"),
            "--merge-output-format".into(),
            "mkv".into(),
            "--".into(),
            url,
        ];
        let out = self.capture(&args, Duration::from_secs(90)).await?;
        let data: Value = serde_json::from_slice(&out).context("YTDLP_METADATA_INVALID")?;
        parse_probe(&data, max_height)
    }
    async fn health(&self) -> Result<String> {
        let out = self
            .capture(&["--version".into()], Duration::from_secs(10))
            .await?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }
    async fn inspect(&self, url: &str, limit: u32) -> Result<Inspection> {
        let url = youtube_url(url).map_err(anyhow::Error::new)?;
        self.metadata(&url, limit).await
    }
    async fn search(&self, query: &str, limit: u32) -> Result<Inspection> {
        if query.len() > 200 || query.trim().is_empty() {
            bail!("SEARCH_QUERY_INVALID");
        }
        self.metadata(
            &format!("ytsearch{}:{}", limit.clamp(1, 20), query),
            limit.clamp(1, 20),
        )
        .await
    }
}
async fn terminate(child: &mut tokio::process::Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    if let Some(id) = child.id() {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut kill = Command::new("taskkill");
            kill.args(["/PID", &id.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            kill.as_std_mut().creation_flags(0x08000000);
            let _ = tokio::time::timeout(Duration::from_secs(5), kill.status()).await;
        }
        #[cfg(unix)]
        {
            // Every yt-dlp invocation owns a new process group. Kill that group,
            // including FFmpeg/Deno children, rather than leaving writers alive.
            unsafe {
                libc::kill(-(id as i32), libc::SIGKILL);
            }
        }
    }
    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_format_height_is_real_and_muxed_sizes_are_not_exact() {
        let data = serde_json::json!({"id":"dQw4w9WgXcQ","title":"Episode","channel_id":"UCaaaaaaaaaaaaaaaaaaaaaa","formats":[{"height":4320}],"requested_formats":[
            {"height":720,"vcodec":"avc1","filesize":1000},{"vcodec":"none","acodec":"opus","filesize":100}]});
        let probe = parse_probe(&data, 1080).unwrap();
        assert_eq!(probe.height, 720);
        assert_eq!(probe.size_bytes, None);
        assert_eq!(probe.container.as_deref(), Some("mkv"));
        assert!(parse_probe(&data, 480).is_err());
        let mut missing = data;
        missing["requested_formats"][0]
            .as_object_mut()
            .unwrap()
            .remove("height");
        assert!(parse_probe(&missing, 1080).is_err());
    }
}
