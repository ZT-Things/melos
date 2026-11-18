//! Small LRC parser shared by the daemon and the TUI.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    pub timestamp_ms: u64,
    pub text: String,
}

/// Parse `[mm:ss.xx]` or `[hh:mm:ss.xx]` timestamps.
///
/// Metadata tags such as `[ar:...]` are ignored. Multiple timestamps on one
/// line produce one entry each, and the result is sorted by time.
pub fn parse_lrc(input: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for raw in input.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else {
                break;
            };
            let tag = &stripped[..end];
            if let Some(ms) = parse_timestamp(tag) {
                stamps.push(ms);
            }
            rest = stripped[end + 1..].trim_start();
        }
        let text = rest.trim();
        if text.is_empty() {
            continue;
        }
        for timestamp_ms in stamps {
            lines.push(LyricLine {
                timestamp_ms,
                text: text.to_string(),
            });
        }
    }
    lines.sort_by_key(|line| line.timestamp_ms);
    lines
}

/// Strip LRC timestamps so the same text can be shown as plain lyrics.
pub fn plain_from_lrc(input: &str) -> String {
    parse_lrc(input)
        .into_iter()
        .map(|line| line.text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Index of the last line whose timestamp has been reached.
///
/// The offset is applied in the same direction as playback: positive values
/// show lyrics earlier.
pub fn current_line(lines: &[LyricLine], position_ms: u64, offset_ms: i64) -> Option<usize> {
    let position = position_ms as i128 + offset_ms as i128;
    if position < 0 {
        return None;
    }
    lines
        .iter()
        .rposition(|line| line.timestamp_ms as i128 <= position)
}

pub fn has_timestamps(input: &str) -> bool {
    !parse_lrc(input).is_empty()
}

fn parse_timestamp(tag: &str) -> Option<u64> {
    let mut parts = tag.split(':');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let (hours, minutes, seconds) = match third {
        Some(seconds) => (
            first.parse::<u64>().ok()?,
            second.parse::<u64>().ok()?,
            seconds,
        ),
        None => (0, first.parse::<u64>().ok()?, second),
    };
    let seconds: f64 = seconds.parse().ok()?;
    if !(0.0..60.0).contains(&seconds) || minutes >= 60 {
        return None;
    }
    Some(
        hours * 3_600_000
            + minutes * 60_000
            + (seconds * 1000.0).round().clamp(0.0, 59_999.0) as u64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_metadata_and_handles_hours() {
        let lines = parse_lrc("[ar:Someone]\n[01:02:03.50]Long\nnot a timestamp");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].timestamp_ms, 3_723_500);
        assert!(!has_timestamps("[ar:Someone]\nplain"));
    }

    #[test]
    fn plain_text_is_not_synced() {
        assert!(!has_timestamps("just words\nno times"));
        assert!(has_timestamps("[00:01.00]words"));
    }
}
