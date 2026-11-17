//! Wire types shared by the melos daemon and TUI client.
//!
//! The daemon is authoritative: the client forwards [`Action`]s and renders the
//! [`ViewModel`] it receives. Version is bumped on any breaking wire change.

pub mod config;
pub mod paths;

use serde::{Deserialize, Deserializer, Serialize};

/// Bump on every breaking change to the wire format.
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Transport / UI model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn cycle(self) -> Self {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackRow {
    pub id: i64,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub missing: bool,
}

impl TrackRow {
    pub fn duration_label(&self) -> String {
        format_duration(self.duration_ms)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaylistRow {
    pub id: i64,
    pub name: String,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transport {
    pub track: Option<TrackRow>,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub playing: bool,
    pub volume: i64,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    #[default]
    Queued,
    Resolving,
    Downloading,
    Postprocessing,
    Done,
    Failed,
    Cancelled,
}

impl DownloadState {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            DownloadState::Done | DownloadState::Failed | DownloadState::Cancelled
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            DownloadState::Queued => "queued",
            DownloadState::Resolving => "resolving",
            DownloadState::Downloading => "downloading",
            DownloadState::Postprocessing => "processing",
            DownloadState::Done => "done",
            DownloadState::Failed => "failed",
            DownloadState::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadRow {
    pub id: u64,
    pub title: String,
    pub state: DownloadState,
    pub percent: f32,
    pub detail: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct YtResultRow {
    pub video_id: String,
    pub title: String,
    pub uploader: String,
    pub duration_s: u64,
    pub url: String,
}

impl YtResultRow {
    pub fn duration_label(&self) -> String {
        format_duration(self.duration_s * 1000)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewModel {
    pub daemon_version: String,
    pub protocol: u32,
    pub tracks: Vec<TrackRow>,
    pub queue: Vec<TrackRow>,
    pub queue_pos: Option<usize>,
    pub playlists: Vec<PlaylistRow>,
    pub playlist_tracks: Vec<TrackRow>,
    pub selected_playlist: Option<i64>,
    pub downloads: Vec<DownloadRow>,
    pub yt_results: Vec<YtResultRow>,
    pub transport: Transport,
    pub status: String,
}

impl Default for ViewModel {
    fn default() -> Self {
        Self {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            tracks: Vec::new(),
            queue: Vec::new(),
            queue_pos: None,
            playlists: Vec::new(),
            playlist_tracks: Vec::new(),
            selected_playlist: None,
            downloads: Vec::new(),
            yt_results: Vec::new(),
            transport: Transport::default(),
            status: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Actions (client -> daemon)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    PlayTrack {
        track_id: i64,
        #[serde(default)]
        context: Vec<i64>,
    },
    PlayQueueIndex {
        index: usize,
    },
    PauseToggle,
    SetPaused {
        paused: bool,
    },
    Stop,
    Next,
    Prev,
    SeekBy {
        seconds: i64,
    },
    SeekTo {
        seconds: u64,
    },
    SetVolume {
        volume: i64,
    },
    VolumeDelta {
        delta: i64,
    },
    MuteToggle,
    ShuffleToggle,
    SetShuffle {
        on: bool,
    },
    RepeatCycle,
    SetRepeat {
        mode: Repeat,
    },

    QueueTrack {
        track_id: i64,
    },
    QueueRemove {
        index: usize,
    },
    QueueClear,

    LibraryRefresh,
    CreatePlaylist {
        name: String,
    },
    DeletePlaylist {
        playlist_id: i64,
    },
    SelectPlaylist {
        playlist_id: Option<i64>,
    },
    AddToPlaylist {
        playlist_id: i64,
        track_id: i64,
    },
    RemoveFromPlaylist {
        playlist_id: i64,
        track_id: i64,
    },
    PlayPlaylist {
        playlist_id: i64,
    },

    YtSearch {
        query: String,
    },
    Download {
        url: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        uploader: Option<String>,
    },
    CancelDownload {
        id: u64,
    },
    ClearFinished,

    Shutdown,
}

// ---------------------------------------------------------------------------
// Wire messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello { protocol: u32, client: String },
    Action { action: Action },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        protocol: u32,
        daemon_version: String,
        snapshot: ViewModel,
    },
    State {
        snapshot: ViewModel,
    },
    Transport {
        transport: Transport,
    },
    Toast {
        text: String,
    },
    Error {
        message: String,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn format_duration(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Trim a string for display without breaking on char boundaries.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Deserializer helper for older state files.
pub fn de_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(61_000), "1:01");
        assert_eq!(format_duration(3_723_000), "1:02:03");
    }

    #[test]
    fn action_roundtrip() {
        let acts = [
            Action::PauseToggle,
            Action::PlayTrack {
                track_id: 7,
                context: vec![1, 2, 3],
            },
            Action::YtSearch {
                query: "daft punk".into(),
            },
            Action::Download {
                url: "https://example.com".into(),
                title: None,
                uploader: None,
            },
        ];
        for a in acts {
            let s = serde_json::to_string(&a).unwrap();
            let back: Action = serde_json::from_str(&s).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), s);
        }
    }

    #[test]
    fn msg_roundtrip() {
        let m = ClientMsg::Hello {
            protocol: PROTOCOL_VERSION,
            client: "test".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"type\":\"hello\""));
        let _: ClientMsg = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 5), "abcd…");
        assert_eq!(truncate("éééééé", 4), "ééé…");
    }
}
