//! LRCLIB lookup and YouTube-title cleaning for lyrics.

use std::time::Duration;

use melos_proto::TrackRow;
use serde_json::Value;

pub const USER_AGENT: &str = concat!(
    "melos/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/ZT-Things/melos)"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedLyrics {
    pub plain: String,
    pub synced: Option<String>,
    pub source: String,
}

pub fn fetch(track: &TrackRow) -> Result<FetchedLyrics, String> {
    let title = clean_title(&track.title);
    let artist = clean_artist(&track.artist);
    if title.is_empty() {
        return Err("this track has no title to search for".into());
    }

    let mut exact = format!(
        "https://lrclib.net/api/get?track_name={}",
        urlencoding::encode(&title)
    );
    if !artist.is_empty() {
        exact.push_str(&format!("&artist_name={}", urlencoding::encode(&artist)));
    }
    if let Some(album) = track.album.as_deref().filter(|a| !a.trim().is_empty()) {
        exact.push_str(&format!(
            "&album_name={}",
            urlencoding::encode(album.trim())
        ));
    }
    exact.push_str(&format!("&duration={}", track.duration_ms / 1000));

    let (status, retry_after, body) = fetch_url(&exact)?;
    if status == 200
        && let Ok(value) = serde_json::from_str::<Value>(&body)
        && let Some(found) = parse_record(&value)
    {
        return Ok(found);
    }
    status_error(status, retry_after.as_deref())?;

    let mut query = title.clone();
    if !artist.is_empty() {
        query.push(' ');
        query.push_str(&artist);
    }
    let search = format!(
        "https://lrclib.net/api/search?q={}",
        urlencoding::encode(query.trim())
    );
    let (status, retry_after, body) = fetch_url(&search)?;
    status_error(status, retry_after.as_deref())?;
    let records: Vec<Value> =
        serde_json::from_str(&body).map_err(|e| format!("invalid LRCLIB response: {e}"))?;
    pick_best(&records, track).ok_or_else(|| NO_LYRICS.to_string())
}

pub fn clean_title(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input.trim();
    while let Some(start) = rest.find(['【', '[', '(']) {
        let open = rest[start..].chars().next().unwrap_or(' ');
        let close = match open {
            '【' => '】',
            '[' => ']',
            '(' => ')',
            _ => continue,
        };
        let Some(end) = rest[start..].find(close).map(|i| start + i) else {
            break;
        };
        out.push_str(&rest[..start]);
        let inside = &rest[start + open.len_utf8()..end];
        if !is_noise_segment(inside) {
            out.push_str(&rest[start..end + close.len_utf8()]);
        }
        rest = &rest[end + close.len_utf8()..];
    }
    out.push_str(rest);
    let mut out = out.trim().to_string();
    for marker in [
        " - official video",
        " - official audio",
        " - official music video",
        " - lyrics",
        " - lyric video",
    ] {
        let lower = out.to_ascii_lowercase();
        if let Some(pos) = lower.find(marker) {
            out.truncate(pos);
        }
    }
    collapse_whitespace(out.trim())
}

pub fn clean_artist(input: &str) -> String {
    let mut out = input.trim().to_string();
    for suffix in [" - topic", " - official", " vevo"] {
        let lower = out.to_ascii_lowercase();
        if lower.ends_with(suffix) {
            out.truncate(out.len() - suffix.len());
            break;
        }
    }
    collapse_whitespace(out.trim())
}

fn is_noise_segment(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    [
        "official",
        "video",
        "audio",
        "lyrics",
        "lyric",
        "visualizer",
        "visualiser",
        "music video",
        "mv",
        "m/v",
        "hd",
        "4k",
        "8d",
        "remaster",
        "royalty free",
        "short ver",
        "full ver",
        "color coded",
        "eng sub",
        "romaji",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_record(value: &Value) -> Option<FetchedLyrics> {
    let plain = value
        .get("plainLyrics")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let synced = value
        .get("syncedLyrics")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let plain = if plain.is_empty() {
        synced
            .as_deref()
            .map(melos_proto::lyrics::plain_from_lrc)
            .unwrap_or_default()
    } else {
        plain
    };
    if plain.is_empty() && synced.is_none() {
        return None;
    }
    Some(FetchedLyrics {
        plain,
        synced,
        source: "lrclib".into(),
    })
}

fn pick_best(records: &[Value], track: &TrackRow) -> Option<FetchedLyrics> {
    let title = clean_title(&track.title).to_ascii_lowercase();
    let artist = clean_artist(&track.artist).to_ascii_lowercase();
    let duration = track.duration_ms as f64 / 1000.0;
    let mut best: Option<(f64, FetchedLyrics)> = None;
    for value in records {
        let Some(found) = parse_record(value) else {
            continue;
        };
        let record_title = value
            .get("trackName")
            .and_then(Value::as_str)
            .or_else(|| value.get("name").and_then(Value::as_str))
            .unwrap_or("")
            .to_ascii_lowercase();
        let record_artist = value
            .get("artistName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let record_duration = value
            .get("duration")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        let mut score = 0.0;
        if record_title == title {
            score += 30.0;
        } else if !title.is_empty() && record_title.contains(&title) {
            score += 15.0;
        } else if !record_title.is_empty() && title.contains(&record_title) {
            score += 10.0;
        }
        if !artist.is_empty() {
            if record_artist == artist {
                score += 25.0;
            } else if record_artist.contains(&artist) || artist.contains(&record_artist) {
                score += 12.0;
            } else {
                score -= 12.0;
            }
        }
        let diff = (record_duration - duration).abs();
        if diff <= 2.0 {
            score += 40.0;
        } else if diff <= 8.0 {
            score += 20.0;
        } else if diff <= 20.0 {
            score += 5.0;
        } else {
            score -= 20.0;
        }
        if found.synced.is_some() {
            score += 5.0;
        }
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, found));
        }
    }
    best.filter(|(score, _)| *score > 0.0)
        .map(|(_, found)| found)
}

const NO_LYRICS: &str = "no lyrics found on LRCLIB";

fn status_error(status: u16, retry_after: Option<&str>) -> Result<(), String> {
    match status {
        200..=299 | 404 => Ok(()),
        429 => Err(match retry_after {
            Some(seconds) => format!("LRCLIB rate limited; retry after {seconds} seconds"),
            None => "LRCLIB rate limited; try again in a minute".into(),
        }),
        other => Err(format!("LRCLIB returned HTTP {other}")),
    }
}

fn fetch_url(url: &str) -> Result<(u16, Option<String>, String), String> {
    let mut response = ureq::get(url)
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("LRCLIB request failed: {e}"))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("could not read LRCLIB response: {e}"))?;
    Ok((status, retry_after, body))
}

const TIMEOUT: Duration = Duration::from_secs(8);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn track() -> TrackRow {
        TrackRow {
            id: 10,
            title: "Neurotoxin - Evil (Official Video)".into(),
            artist: "Neuro-sama".into(),
            album: None,
            duration_ms: 210_014,
            missing: false,
        }
    }

    fn record(
        track_name: &str,
        artist_name: &str,
        duration: f64,
        plain: &str,
        synced: Option<&str>,
    ) -> Value {
        json!({
            "trackName": track_name,
            "artistName": artist_name,
            "duration": duration,
            "plainLyrics": plain,
            "syncedLyrics": synced,
        })
    }

    #[test]
    fn cleans_youtube_title_noise() {
        assert_eq!(
            clean_title("Neurotoxin - Evil (Official Video)"),
            "Neurotoxin - Evil"
        );
        assert_eq!(clean_title("【MV】DYNA MIND"), "DYNA MIND");
        assert_eq!(
            clean_title("A Short Walk With You [Royalty Free]"),
            "A Short Walk With You"
        );
        assert_eq!(clean_title("Song (feat. Someone)"), "Song (feat. Someone)");
        assert_eq!(clean_artist("Neuro-sama - Topic"), "Neuro-sama");
    }

    #[test]
    fn parses_synced_and_plain_records() {
        let v = record(
            "Song",
            "Artist",
            180.0,
            "line one\nline two",
            Some("[00:01.00]line one\n[00:02.00]line two"),
        );
        let parsed = parse_record(&v).expect("lyrics");
        assert_eq!(parsed.plain, "line one\nline two");
        assert!(parsed.synced.is_some());
        assert_eq!(parsed.source, "lrclib");
    }

    #[test]
    fn derives_plain_text_when_only_lrc_is_present() {
        let v = record(
            "Song",
            "Artist",
            180.0,
            "",
            Some("[00:01.00]line one\n[00:02.00]line two"),
        );
        let parsed = parse_record(&v).expect("lyrics");
        assert_eq!(parsed.plain, "line one\nline two");
    }

    #[test]
    fn ignores_instrumental_and_empty_records() {
        let mut v = record("Song", "Artist", 180.0, "", None);
        v["instrumental"] = json!(true);
        assert!(parse_record(&v).is_none());
    }

    #[test]
    fn picks_the_closest_duration_and_matching_artist() {
        let records = vec![
            record("Neurotoxin", "Neurotoxin", 420.0, "wrong", None),
            record(
                "Neurotoxin",
                "Neuro-sama",
                210.0,
                "right",
                Some("[00:01.00]right"),
            ),
            record("Neurotoxin", "Neuro-sama", 300.0, "too long", None),
        ];
        let best = pick_best(&records, &track()).expect("best");
        assert_eq!(best.plain, "right");
        assert!(best.synced.is_some());
    }

    #[test]
    fn explains_missing_and_rate_limited_responses() {
        assert!(status_error(404, None).is_ok());
        assert!(NO_LYRICS.contains("no lyrics"));
        assert!(status_error(429, Some("12")).unwrap_err().contains("12"));
        assert!(status_error(200, None).is_ok());
    }
}
