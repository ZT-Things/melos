//! yt-dlp integration: search, download, progress reporting, tag ingestion.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use melos_proto::{DownloadRow, DownloadState, YtResultRow, config::Config};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Semaphore, mpsc};

use crate::db::NewTrack;

pub enum Event {
    SearchResults(Vec<YtResultRow>),
    SearchFailed(String),
    JobUpdate(DownloadRow),
    Complete { id: u64, track: NewTrack },
    Failed { id: u64, error: String },
}

fn base_command(cfg: &Config) -> Command {
    let mut cmd = Command::new("yt-dlp");
    cmd.arg("--no-warnings");
    if !cfg.download.cookies_from_browser.is_empty() {
        cmd.arg("--cookies-from-browser")
            .arg(&cfg.download.cookies_from_browser);
    }
    if !cfg.download.proxy.is_empty() {
        cmd.arg("--proxy").arg(&cfg.download.proxy);
    }
    for arg in &cfg.download.extra_args {
        cmd.arg(arg);
    }
    cmd
}

pub async fn search(
    query: String,
    cfg: Arc<Config>,
    sem: Arc<Semaphore>,
    tx: mpsc::UnboundedSender<Event>,
) {
    let permit = sem.acquire_owned().await;
    if permit.is_err() {
        return;
    }
    let result = search_inner(&query, &cfg).await;
    match result {
        Ok(results) => {
            let _ = tx.send(Event::SearchResults(results));
        }
        Err(e) => {
            let _ = tx.send(Event::SearchFailed(e.to_string()));
        }
    }
}

async fn search_inner(query: &str, cfg: &Config) -> Result<Vec<YtResultRow>> {
    let mut cmd = base_command(cfg);
    cmd.arg("--flat-playlist")
        .arg("--dump-single-json")
        .arg(format!("ytsearch10:{query}"))
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let output = cmd.output().await.context("running yt-dlp search")?;
    if !output.status.success() {
        anyhow::bail!("yt-dlp search failed (is yt-dlp installed and up to date?)");
    }
    let json: Value = serde_json::from_slice(&output.stdout).context("parsing yt-dlp search")?;
    let entries = json
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut results = Vec::new();
    for entry in entries {
        let video_id = entry
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let title = entry
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("(untitled)")
            .to_string();
        let uploader = entry
            .get("uploader")
            .or_else(|| entry.get("channel"))
            .and_then(Value::as_str)
            .unwrap_or("Unknown")
            .to_string();
        let duration_s = entry
            .get("duration")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .max(0.0) as u64;
        let url = entry
            .get("webpage_url")
            .or_else(|| entry.get("url"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={video_id}"));
        if video_id.is_empty() {
            continue;
        }
        results.push(YtResultRow {
            video_id,
            title,
            uploader,
            duration_s,
            url,
        });
    }
    Ok(results)
}

#[allow(clippy::too_many_arguments)]
pub async fn download(
    id: u64,
    url: String,
    title: Option<String>,
    uploader: Option<String>,
    cfg: Arc<Config>,
    sem: Arc<Semaphore>,
    tx: mpsc::UnboundedSender<Event>,
) {
    let mut row = DownloadRow {
        id,
        title: title.clone().unwrap_or_else(|| url.clone()),
        state: DownloadState::Queued,
        percent: 0.0,
        detail: "waiting for a slot".into(),
        error: None,
    };
    let _ = tx.send(Event::JobUpdate(row.clone()));

    let permit = sem.acquire_owned().await;
    if permit.is_err() {
        return;
    }

    row.state = DownloadState::Downloading;
    row.detail = "starting".into();
    let _ = tx.send(Event::JobUpdate(row.clone()));

    match download_inner(id, &url, uploader.as_deref(), &cfg, &tx, &mut row).await {
        Ok(track) => {
            row.state = DownloadState::Postprocessing;
            row.percent = 100.0;
            row.detail = "adding to library".into();
            let _ = tx.send(Event::JobUpdate(row));
            let _ = tx.send(Event::Complete { id, track });
        }
        Err(e) => {
            row.state = DownloadState::Failed;
            row.error = Some(e.to_string());
            row.detail = "failed".into();
            let _ = tx.send(Event::JobUpdate(row));
            let _ = tx.send(Event::Failed {
                id,
                error: e.to_string(),
            });
        }
    }
}

async fn download_inner(
    id: u64,
    url: &str,
    uploader: Option<&str>,
    cfg: &Config,
    tx: &mpsc::UnboundedSender<Event>,
    row: &mut DownloadRow,
) -> Result<NewTrack> {
    let music_dir = cfg.music_dir_path();
    std::fs::create_dir_all(&music_dir)
        .with_context(|| format!("creating {}", music_dir.display()))?;

    let err_path = melos_proto::paths::state_dir().join(format!("download-{id}.log"));
    if let Some(parent) = err_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let err_file = std::fs::File::create(&err_path).context("creating yt-dlp log")?;

    let template = music_dir
        .join("%(uploader,channel,creator|Unknown)s/%(title)s [%(id)s].%(ext)s")
        .to_string_lossy()
        .to_string();

    let mut cmd = base_command(cfg);
    cmd.arg("--no-playlist")
        .arg("-x")
        .arg("--audio-format")
        .arg(&cfg.audio_format)
        .arg("--audio-quality")
        .arg("0")
        .arg("--embed-metadata")
        .arg("--embed-thumbnail")
        .arg("--newline")
        .arg("--progress")
        .arg("--progress-template")
        .arg("melos:%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s")
        .arg("--print")
        .arg("after_move:filepath")
        .arg("-o")
        .arg(&template)
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(err_file))
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("starting yt-dlp")?;
    let stdout = child.stdout.take().context("capturing yt-dlp output")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut last_path: Option<String> = None;
    let mut last_pct: i64 = -1;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("melos:") {
            let mut parts = rest.splitn(3, '|');
            let pct_raw = parts.next().unwrap_or("").trim();
            let speed = parts.next().unwrap_or("").trim();
            let eta = parts.next().unwrap_or("").trim();
            let pct = pct_raw
                .trim_end_matches('%')
                .trim()
                .parse::<f32>()
                .unwrap_or(0.0);
            let rounded = pct.round() as i64;
            if rounded != last_pct {
                last_pct = rounded;
                row.percent = pct;
                row.detail = format!("{}  {speed} {eta}", pct_raw.trim());
                let _ = tx.send(Event::JobUpdate(row.clone()));
            }
        } else {
            last_path = Some(line.to_string());
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        let tail = tail_of(&err_path, 500).unwrap_or_default();
        anyhow::bail!("yt-dlp exited with {status}: {tail}");
    }

    let path = last_path
        .or_else(|| find_newest_file(&music_dir).map(|p| p.to_string_lossy().to_string()))
        .context("yt-dlp did not report an output path")?;
    let _ = std::fs::remove_file(&err_path);

    row.state = DownloadState::Postprocessing;
    row.detail = "reading tags".into();
    let _ = tx.send(Event::JobUpdate(row.clone()));

    let uploader = uploader.map(str::to_string);
    tokio::task::spawn_blocking(move || ingest(&path, uploader.as_deref()))
        .await
        .context("tag reader panicked")?
}

fn ingest(path: &str, uploader_hint: Option<&str>) -> Result<NewTrack> {
    let p = PathBuf::from(path);
    let meta = std::fs::metadata(&p).with_context(|| format!("stat {path}"))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let yt_id = parse_yt_id(&stem).unwrap_or_else(|| stem.clone());

    let mut title = stem.clone();
    let mut artist = uploader_hint.unwrap_or("Unknown").to_string();
    if let Some(prefix) = title.rsplit_once(" [").map(|(a, _)| a.to_string())
        && !prefix.is_empty()
    {
        title = prefix;
    }
    let mut album = None;
    let mut duration_ms = 0;

    if let Ok(tagged) = lofty::read_from_path(&p) {
        use lofty::file::{AudioFile, TaggedFileExt};
        use lofty::tag::Accessor;

        duration_ms = tagged.properties().duration().as_millis() as u64;
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(t) = tag.title().filter(|s| !s.trim().is_empty()) {
                title = t.to_string();
            }
            if let Some(a) = tag.artist().filter(|s| !s.trim().is_empty()) {
                artist = a.to_string();
            }
            album = tag.album().map(|a| a.to_string());
        }
    }

    Ok(NewTrack {
        path: path.to_string(),
        yt_id,
        title,
        artist,
        album,
        duration_ms,
        mtime,
    })
}

fn parse_yt_id(stem: &str) -> Option<String> {
    let start = stem.rfind('[')?;
    let end = stem.rfind(']')?;
    if end <= start + 1 {
        return None;
    }
    let id = &stem[start + 1..end];
    if id.is_empty()
        || id.len() > 32
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(id.to_string())
}

fn tail_of(path: &std::path::Path, max_chars: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let tail: String = trimmed
        .chars()
        .rev()
        .take(max_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Some(tail.replace('\n', " | "))
}

fn find_newest_file(dir: &std::path::Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Ok(meta) = entry.metadata()
                && let Ok(modified) = meta.modified()
                && best.as_ref().is_none_or(|(t, _)| modified > *t)
            {
                best = Some((modified, path));
            }
        } else if path.is_dir()
            && let Some(found) = find_newest_file(&path)
            && let Ok(meta) = std::fs::metadata(&found)
            && let Ok(modified) = meta.modified()
            && best.as_ref().is_none_or(|(t, _)| modified > *t)
        {
            best = Some((modified, found));
        }
    }
    best.map(|(_, p)| p)
}

/// Warn when yt-dlp is ancient; the version string is `YYYY.MM.DD`.
pub async fn yt_dlp_age_days() -> Option<i64> {
    let output = Command::new("yt-dlp")
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let mut parts = version.trim().split('.');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    let built = days_from_civil(y, m, d);
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64
        / 86_400;
    Some(today - built)
}

/// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yt_id_parsing() {
        assert_eq!(
            parse_yt_id("Song Title [dQw4w9WgXcQ]"),
            Some("dQw4w9WgXcQ".into())
        );
        assert_eq!(parse_yt_id("no id here"), None);
        assert_eq!(parse_yt_id("bad [a b]"), None);
    }

    #[test]
    fn civil_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
    }
}
