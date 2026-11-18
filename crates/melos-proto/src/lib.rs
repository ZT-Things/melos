//! Wire types shared by the melos daemon and TUI client.
//!
//! The daemon is authoritative: the client forwards [`Action`]s and renders the
//! [`ViewModel`] it receives. Version is bumped on any breaking wire change.

pub mod config;
pub mod lyrics;
pub mod paths;

use serde::{Deserialize, Deserializer, Serialize};

/// Bump on every breaking change to the wire format.
pub const PROTOCOL_VERSION: u32 = 4;

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
    /// Absolute path to cached cover art, if the daemon extracted one.
    #[serde(default)]
    pub art_path: Option<String>,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub playing: bool,
    pub volume: i64,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    pub total_ms: u64,
    pub today_ms: u64,
    pub week_ms: u64,
    /// Seven days, oldest first.
    pub days: Vec<DayStat>,
    pub top_artists: Vec<NameStat>,
    pub top_tracks: Vec<TrackStat>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DayStat {
    pub day: String,
    pub ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NameStat {
    pub name: String,
    pub ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackStat {
    pub title: String,
    pub artist: String,
    pub ms: u64,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricsView {
    pub track_id: i64,
    pub plain: String,
    pub synced: Option<String>,
    pub source: String,
    pub offset_ms: i64,
    pub updated_at: i64,
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
    #[serde(default)]
    pub stats: Stats,
    #[serde(default)]
    pub lyrics: Option<LyricsView>,
    #[serde(default)]
    pub lyrics_loading: bool,
    #[serde(default)]
    pub lyrics_error: Option<String>,
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
            stats: Stats::default(),
            lyrics: None,
            lyrics_loading: false,
            lyrics_error: None,
            status: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Actions (client -> daemon)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    MoveQueue {
        start: usize,
        len: usize,
        delta: i8,
    },
    MoveLibrary {
        start: usize,
        len: usize,
        delta: i8,
    },
    MovePlaylistTracks {
        playlist_id: i64,
        start: usize,
        len: usize,
        delta: i8,
    },

    LibraryRefresh,
    DeleteTrack {
        track_id: i64,
        #[serde(default)]
        delete_file: bool,
    },
    RenameTrack {
        track_id: i64,
        title: String,
    },
    RenamePlaylist {
        playlist_id: i64,
        name: String,
    },
    RequestStats,
    RequestLyrics {
        #[serde(default)]
        force: bool,
    },
    SaveLyrics {
        track_id: i64,
        text: String,
    },
    SetLyricsOffset {
        track_id: i64,
        offset_ms: i64,
    },
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
    Lyrics {
        track_id: i64,
        #[serde(default)]
        lyrics: Option<LyricsView>,
        #[serde(default)]
        loading: bool,
        #[serde(default)]
        error: Option<String>,
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
            Action::DeleteTrack {
                track_id: 4,
                delete_file: true,
            },
            Action::RenameTrack {
                track_id: 4,
                title: "New title".into(),
            },
            Action::RenamePlaylist {
                playlist_id: 2,
                name: "Focus".into(),
            },
            Action::RequestStats,
            Action::RequestLyrics { force: true },
            Action::SaveLyrics {
                track_id: 4,
                text: "[00:01.00]Hello".into(),
            },
            Action::SetLyricsOffset {
                track_id: 4,
                offset_ms: -500,
            },
            Action::MoveLibrary {
                start: 1,
                len: 2,
                delta: 1,
            },
            Action::MovePlaylistTracks {
                playlist_id: 2,
                start: 3,
                len: 1,
                delta: -1,
            },
            Action::MoveQueue {
                start: 0,
                len: 2,
                delta: 1,
            },
        ];
        for a in acts {
            let s = serde_json::to_string(&a).unwrap();
            let back: Action = serde_json::from_str(&s).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), s);
        }
    }

    #[test]
    fn stats_and_art_default_to_empty_on_older_messages() {
        let transport: Transport = serde_json::from_str(
            r#"{"track":null,"position_ms":0,"duration_ms":0,"playing":false,
                "volume":70,"muted":false,"shuffle":false,"repeat":"off"}"#,
        )
        .unwrap();
        assert_eq!(transport.art_path, None);
        let vm: ViewModel = serde_json::from_str(
            r#"{"daemon_version":"0.1.0","protocol":1,"tracks":[],"queue":[],"queue_pos":null,
                "playlists":[],"playlist_tracks":[],"selected_playlist":null,"downloads":[],
                "yt_results":[],"transport":{"track":null,"position_ms":0,"duration_ms":0,
                "playing":false,"volume":70,"muted":false,"shuffle":false,"repeat":"off"},
                "status":""}"#,
        )
        .unwrap();
        assert_eq!(vm.stats.total_ms, 0);
        assert!(vm.stats.days.is_empty());
        assert!(vm.lyrics.is_none());
        assert!(!vm.lyrics_loading);
        assert!(vm.lyrics_error.is_none());
    }

    #[test]
    fn lyrics_message_roundtrip() {
        let msg = ServerMsg::Lyrics {
            track_id: 4,
            lyrics: Some(LyricsView {
                track_id: 4,
                plain: "Hello".into(),
                synced: Some("[00:01.00]Hello".into()),
                source: "manual".into(),
                offset_ms: -250,
                updated_at: 1,
            }),
            loading: false,
            error: None,
        };
        let s = serde_json::to_string(&msg).unwrap();
        let back: ServerMsg = serde_json::from_str(&s).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), s);
    }

    #[test]
    fn parses_lrc_and_finds_the_current_line() {
        let lrc =
            "[ar:Artist]\n[00:01.00]First line\n[00:05.50][00:09.00]Chorus\n[00:12.25] Last line ";
        let lines = crate::lyrics::parse_lrc(lrc);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].timestamp_ms, 1_000);
        assert_eq!(lines[0].text, "First line");
        assert_eq!(lines[1].timestamp_ms, 5_500);
        assert_eq!(lines[1].text, "Chorus");
        assert_eq!(lines[2].timestamp_ms, 9_000);
        assert_eq!(lines[3].text, "Last line");
        assert_eq!(crate::lyrics::current_line(&lines, 6_000, 0), Some(1));
        assert_eq!(crate::lyrics::current_line(&lines, 6_000, 3_500), Some(2));
        assert_eq!(crate::lyrics::current_line(&lines, 500, 0), None);
        assert_eq!(
            crate::lyrics::plain_from_lrc(lrc),
            "First line\nChorus\nChorus\nLast line"
        );
    }

    #[test]
    fn stats_serialize_day_and_top_lists() {
        let stats = Stats {
            total_ms: 90_000,
            today_ms: 30_000,
            week_ms: 60_000,
            days: vec![DayStat {
                day: "2026-11-17".into(),
                ms: 30_000,
            }],
            top_artists: vec![NameStat {
                name: "Lamp".into(),
                ms: 30_000,
            }],
            top_tracks: vec![TrackStat {
                title: "Song".into(),
                artist: "Lamp".into(),
                ms: 30_000,
            }],
        };
        let s = serde_json::to_string(&stats).unwrap();
        let back: Stats = serde_json::from_str(&s).unwrap();
        assert_eq!(back.total_ms, 90_000);
        assert_eq!(back.days[0].day, "2026-11-17");
        assert_eq!(back.top_artists[0].name, "Lamp");
        assert_eq!(back.top_tracks[0].title, "Song");
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
