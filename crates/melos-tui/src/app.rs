//! Client-local UI state: view, cursors, filter, input prompts, overlays.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use melos_proto::{
    Action, DownloadState, ServerMsg, TrackRow, ViewModel, format_duration, truncate,
};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::keys::KeyMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Library,
    Playlists,
    PlaylistTracks,
    Queue,
    Downloads,
    Stats,
}

impl View {
    pub fn title(self) -> &'static str {
        match self {
            View::Library => "Library",
            View::Playlists => "Playlists",
            View::PlaylistTracks => "Playlist",
            View::Queue => "Queue",
            View::Downloads => "Downloads",
            View::Stats => "Statistics",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Main,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputKind {
    Filter,
    Download,
    NewPlaylist,
    RenameTrack(i64),
    RenamePlaylist(i64),
}

pub struct Input {
    pub kind: InputKind,
    pub buffer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteChoice {
    Melos,
    File,
}

#[derive(Debug, Clone)]
pub enum Overlay {
    Help,
    PlaylistPicker {
        track_id: i64,
        index: usize,
    },
    DeleteTrack {
        track_id: i64,
        title: String,
        choice: DeleteChoice,
    },
}

pub struct App {
    pub vm: ViewModel,
    pub view: View,
    pub focus: Focus,
    pub sidebar_idx: usize,
    pub main_idx: usize,
    pub filter: String,
    pub input: Option<Input>,
    pub overlay: Option<Overlay>,
    pub keys: KeyMap,
    pub quit: bool,
    pub status_override: Option<String>,
    pub daemon_lost: bool,
    pub daemon_version: String,
    pub picker: Option<Picker>,
    pub art: Option<StatefulProtocol>,
    art_path: Option<String>,
    art_rx: Option<Receiver<(String, StatefulProtocol)>>,
}

const SIDEBAR_VIEWS: [View; 5] = [
    View::Library,
    View::Playlists,
    View::Queue,
    View::Downloads,
    View::Stats,
];

impl App {
    pub fn new(vm: ViewModel, keys: KeyMap) -> Self {
        let daemon_version = vm.daemon_version.clone();
        let art_path = vm.transport.art_path.clone();
        let mut app = Self {
            vm,
            view: View::Library,
            focus: Focus::Main,
            sidebar_idx: 0,
            main_idx: 0,
            filter: String::new(),
            input: None,
            overlay: None,
            keys,
            quit: false,
            status_override: None,
            daemon_lost: false,
            daemon_version,
            picker: None,
            art: None,
            art_path,
            art_rx: None,
        };
        app.reveal_current();
        app
    }

    pub fn set_picker(&mut self, picker: Picker) {
        self.picker = Some(picker);
        self.load_art();
    }

    fn set_art_path(&mut self, path: Option<String>) {
        if self.art_path == path {
            return;
        }
        self.art_path = path;
        self.art = None;
        self.art_rx = None;
        self.load_art();
    }

    fn load_art(&mut self) {
        let (Some(path), Some(picker)) = (self.art_path.clone(), self.picker.clone()) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.art_rx = Some(rx);
        std::thread::spawn(move || {
            if let Ok(image) = image::open(&path) {
                let protocol = picker.new_resize_protocol(image);
                let _ = tx.send((path, protocol));
            }
        });
    }

    pub fn art_pending(&self) -> bool {
        self.art_rx.is_some()
    }

    /// Apply a finished background decode. Called from the draw path so the
    /// event loop never waits on image decoding.
    pub fn poll_art(&mut self) {
        let Some(rx) = self.art_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok((path, protocol)) => {
                if self.art_path.as_deref() == Some(path.as_str()) {
                    self.art = Some(protocol);
                }
                self.art_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.art_rx = None;
            }
        }
    }

    pub fn apply(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Welcome {
                snapshot,
                daemon_version,
                ..
            } => {
                self.daemon_version = daemon_version;
                let art = snapshot.transport.art_path.clone();
                self.vm = snapshot;
                self.set_art_path(art);
            }
            ServerMsg::State { snapshot } => {
                let art = snapshot.transport.art_path.clone();
                self.vm = snapshot;
                self.set_art_path(art);
                if self.view == View::PlaylistTracks && self.vm.selected_playlist.is_none() {
                    self.view = View::Playlists;
                    self.main_idx = 0;
                }
                if !self.daemon_lost {
                    self.status_override = None;
                }
            }
            ServerMsg::Transport { transport } => {
                let art = transport.art_path.clone();
                self.vm.transport = transport;
                self.set_art_path(art);
            }
            ServerMsg::Toast { text } => {
                self.status_override = Some(text);
            }
            ServerMsg::Error { message } => {
                self.status_override = Some(message);
                self.daemon_lost = true;
                self.quit = true;
            }
            ServerMsg::Shutdown => {
                self.daemon_lost = true;
                self.status_override = Some("melosd stopped".into());
                self.quit = true;
            }
        }
        self.clamp_cursor();
    }

    pub fn status_text(&self) -> String {
        self.status_override
            .clone()
            .unwrap_or_else(|| self.vm.status.clone())
    }

    pub fn visible_tracks(&self) -> Vec<&TrackRow> {
        let source = match self.view {
            View::PlaylistTracks => &self.vm.playlist_tracks,
            _ => &self.vm.tracks,
        };
        if self.filter.is_empty() {
            return source.iter().collect();
        }
        source
            .iter()
            .filter(|t| fuzzy_match(&self.filter, &format!("{} {}", t.title, t.artist)))
            .collect()
    }

    pub fn main_len(&self) -> usize {
        match self.view {
            View::Library | View::PlaylistTracks => self.visible_tracks().len(),
            View::Playlists => self.vm.playlists.len(),
            View::Queue => self.vm.queue.len(),
            View::Downloads => self.vm.yt_results.len() + self.vm.downloads.len(),
            View::Stats => 0,
        }
    }

    pub fn clamp_cursor(&mut self) {
        let len = self.main_len();
        if len == 0 {
            self.main_idx = 0;
        } else if self.main_idx >= len {
            self.main_idx = len - 1;
        }
    }

    fn selected_track_id(&self) -> Option<i64> {
        match self.view {
            View::Library | View::PlaylistTracks => {
                self.visible_tracks().get(self.main_idx).map(|t| t.id)
            }
            View::Queue => self.vm.queue.get(self.main_idx).map(|t| t.id),
            _ => None,
        }
    }

    fn context_ids(&self) -> Vec<i64> {
        match self.view {
            View::Library | View::PlaylistTracks => {
                self.visible_tracks().iter().map(|t| t.id).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Put the cursor on the currently playing track, if it is in this view.
    fn reveal_current(&mut self) {
        if self.view != View::Library {
            return;
        }
        let Some(current) = self.vm.transport.track.as_ref().map(|t| t.id) else {
            return;
        };
        if let Some(index) = self
            .visible_tracks()
            .iter()
            .position(|track| track.id == current)
        {
            self.main_idx = index;
        }
    }

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.main_idx = 0;
        self.filter.clear();
        if let Some(input) = &self.input
            && input.kind == InputKind::Filter
        {
            self.input = None;
        }
        self.reveal_current();
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.main_len();
        if len == 0 {
            self.main_idx = 0;
            return;
        }
        let next = self.main_idx as isize + delta;
        self.main_idx = next.clamp(0, len as isize - 1) as usize;
    }

    fn set_cursor(&mut self, index: usize) {
        let len = self.main_len();
        self.main_idx = if len == 0 { 0 } else { index.min(len - 1) };
    }

    fn open_filter(&mut self) {
        if matches!(self.view, View::Library | View::PlaylistTracks) {
            self.input = Some(Input {
                kind: InputKind::Filter,
                buffer: self.filter.clone(),
            });
        }
    }

    fn start_download_prompt(&mut self) {
        self.sidebar_idx = 3;
        self.set_view(View::Downloads);
        self.focus = Focus::Main;
        self.input = Some(Input {
            kind: InputKind::Download,
            buffer: String::new(),
        });
    }

    fn open_playlist_picker(&mut self) {
        let Some(track_id) = self.selected_track_id() else {
            return;
        };
        if self.vm.playlists.is_empty() {
            self.status_override =
                Some("no playlists yet — press N in Playlists to create one".into());
            return;
        }
        self.overlay = Some(Overlay::PlaylistPicker { track_id, index: 0 });
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return Vec::new();
        }
        let mut actions = Vec::new();

        if let Some(mut input) = self.input.take() {
            match key.code {
                KeyCode::Esc => {
                    if input.kind == InputKind::Filter {
                        self.filter.clear();
                    }
                }
                KeyCode::Enter => {
                    let text = input.buffer.trim().to_string();
                    match input.kind.clone() {
                        InputKind::Filter => {
                            self.filter = text;
                            self.main_idx = 0;
                        }
                        InputKind::NewPlaylist => {
                            if !text.is_empty() {
                                actions.push(Action::CreatePlaylist { name: text });
                                self.view = View::Playlists;
                                self.sidebar_idx = 1;
                                self.main_idx = 0;
                            }
                        }
                        InputKind::RenameTrack(track_id) => {
                            if !text.is_empty() {
                                actions.push(Action::RenameTrack {
                                    track_id,
                                    title: text,
                                });
                            }
                        }
                        InputKind::RenamePlaylist(playlist_id) => {
                            if !text.is_empty() {
                                actions.push(Action::RenamePlaylist {
                                    playlist_id,
                                    name: text,
                                });
                            }
                        }
                        InputKind::Download => {
                            if !text.is_empty() {
                                if text.starts_with("http://") || text.starts_with("https://") {
                                    actions.push(Action::Download {
                                        url: text,
                                        title: None,
                                        uploader: None,
                                    });
                                } else {
                                    actions.push(Action::YtSearch { query: text });
                                }
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    input.buffer.pop();
                    if input.kind == InputKind::Filter {
                        self.filter = input.buffer.clone();
                        self.main_idx = 0;
                    }
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    input.buffer.push(c);
                    if input.kind == InputKind::Filter {
                        self.filter = input.buffer.clone();
                        self.main_idx = 0;
                    }
                }
                _ => {}
            }
            // Keep the prompt open unless it was submitted or cancelled.
            let still_open = !matches!(key.code, KeyCode::Enter | KeyCode::Esc);
            if still_open {
                self.input = Some(input);
            }
            return actions;
        }

        if let Some(overlay) = self.overlay.take() {
            match overlay {
                Overlay::Help => { /* any key closes */ }
                Overlay::PlaylistPicker {
                    track_id,
                    mut index,
                } => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Down | KeyCode::Char('j') => {
                        if index + 1 < self.vm.playlists.len() {
                            index += 1;
                        }
                        self.overlay = Some(Overlay::PlaylistPicker { track_id, index });
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        index = index.saturating_sub(1);
                        self.overlay = Some(Overlay::PlaylistPicker { track_id, index });
                    }
                    KeyCode::Enter => {
                        if let Some(playlist) = self.vm.playlists.get(index) {
                            actions.push(Action::AddToPlaylist {
                                playlist_id: playlist.id,
                                track_id,
                            });
                        }
                    }
                    _ => {
                        self.overlay = Some(Overlay::PlaylistPicker { track_id, index });
                    }
                },
                Overlay::DeleteTrack {
                    track_id,
                    title,
                    mut choice,
                } => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Down | KeyCode::Char('j') => {
                        choice = DeleteChoice::File;
                        self.overlay = Some(Overlay::DeleteTrack {
                            track_id,
                            title,
                            choice,
                        });
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        choice = DeleteChoice::Melos;
                        self.overlay = Some(Overlay::DeleteTrack {
                            track_id,
                            title,
                            choice,
                        });
                    }
                    KeyCode::Enter => {
                        actions.push(Action::DeleteTrack {
                            track_id,
                            delete_file: choice == DeleteChoice::File,
                        });
                    }
                    _ => {
                        self.overlay = Some(Overlay::DeleteTrack {
                            track_id,
                            title,
                            choice,
                        });
                    }
                },
            }
            return actions;
        }

        let Some(action) = self.keys.action(key).map(str::to_string) else {
            return actions;
        };

        match action.as_str() {
            "quit" => self.quit = true,
            "shutdown" => {
                actions.push(Action::Shutdown);
                self.quit = true;
            }
            "down" => {
                if self.focus == Focus::Sidebar {
                    self.sidebar_move(1);
                    if self.view == View::Stats {
                        actions.push(Action::RequestStats);
                    }
                } else {
                    self.move_cursor(1);
                }
            }
            "up" => {
                if self.focus == Focus::Sidebar {
                    self.sidebar_move(-1);
                    if self.view == View::Stats {
                        actions.push(Action::RequestStats);
                    }
                } else {
                    self.move_cursor(-1);
                }
            }
            "top" | "bottom" | "half_down" | "half_up" | "page_down" | "page_up" => {
                if self.focus != Focus::Sidebar {
                    match action.as_str() {
                        "top" => self.set_cursor(0),
                        "bottom" => self.set_cursor(self.main_len().saturating_sub(1)),
                        "half_down" | "page_down" => self.move_cursor(10),
                        "half_up" | "page_up" => self.move_cursor(-10),
                        _ => {}
                    }
                }
            }
            "tab" => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Main,
                    Focus::Main => Focus::Sidebar,
                }
            }
            "pane_library" => {
                self.sidebar_idx = 0;
                self.set_view(View::Library);
                self.focus = Focus::Main;
            }
            "pane_playlists" => {
                self.sidebar_idx = 1;
                self.set_view(View::Playlists);
                self.focus = Focus::Main;
            }
            "pane_queue" => {
                self.sidebar_idx = 2;
                self.set_view(View::Queue);
                self.focus = Focus::Main;
            }
            "pane_downloads" => {
                self.sidebar_idx = 3;
                self.set_view(View::Downloads);
                self.focus = Focus::Main;
            }
            "pane_stats" => {
                self.sidebar_idx = 4;
                self.set_view(View::Stats);
                self.focus = Focus::Main;
                actions.push(Action::RequestStats);
            }
            "confirm" => actions.extend(self.activate()),
            "pause" => actions.push(Action::PauseToggle),
            "next" => actions.push(Action::Next),
            "prev" => actions.push(Action::Prev),
            "seek_back" => actions.push(Action::SeekBy { seconds: -5 }),
            "seek_fwd" => actions.push(Action::SeekBy { seconds: 5 }),
            "seek_back_long" => actions.push(Action::SeekBy { seconds: -30 }),
            "seek_fwd_long" => actions.push(Action::SeekBy { seconds: 30 }),
            "volume_up" => actions.push(Action::VolumeDelta { delta: 5 }),
            "volume_down" => actions.push(Action::VolumeDelta { delta: -5 }),
            "mute" => actions.push(Action::MuteToggle),
            "shuffle" => actions.push(Action::ShuffleToggle),
            "repeat" => actions.push(Action::RepeatCycle),
            "queue_add" => {
                if let Some(track_id) = self.selected_track_id()
                    && matches!(self.view, View::Library | View::PlaylistTracks)
                {
                    actions.push(Action::QueueTrack { track_id });
                }
            }
            "playlist_add" => self.open_playlist_picker(),
            "new_playlist" => {
                self.input = Some(Input {
                    kind: InputKind::NewPlaylist,
                    buffer: String::new(),
                });
            }
            "playlist_delete" => {
                if self.view == View::Playlists
                    && let Some(playlist) = self.vm.playlists.get(self.main_idx)
                {
                    actions.push(Action::DeletePlaylist {
                        playlist_id: playlist.id,
                    });
                }
            }
            "remove" => match self.view {
                View::Queue => actions.push(Action::QueueRemove {
                    index: self.main_idx,
                }),
                View::PlaylistTracks => {
                    if let (Some(playlist_id), Some(track)) = (
                        self.vm.selected_playlist,
                        self.visible_tracks().get(self.main_idx).map(|t| t.id),
                    ) {
                        actions.push(Action::RemoveFromPlaylist {
                            playlist_id,
                            track_id: track,
                        });
                    }
                }
                View::Downloads => {
                    let results = self.vm.yt_results.len();
                    if self.main_idx >= results
                        && let Some(job) = self.vm.downloads.get(self.main_idx - results)
                        && !job.state.is_finished()
                    {
                        actions.push(Action::CancelDownload { id: job.id });
                    }
                }
                View::Library => self.open_delete(),
                _ => {}
            },
            "delete_track" => self.open_delete(),
            "rename" => self.start_rename(),
            "filter" => self.open_filter(),
            "help" => self.overlay = Some(Overlay::Help),
            "download" => self.start_download_prompt(),
            "cancel" => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.main_idx = 0;
                } else if self.view == View::PlaylistTracks {
                    self.view = View::Playlists;
                    self.main_idx = 0;
                } else {
                    self.focus = Focus::Sidebar;
                }
            }
            _ => {}
        }
        actions
    }

    fn open_delete(&mut self) {
        if !matches!(self.view, View::Library | View::PlaylistTracks) {
            return;
        }
        let Some((track_id, title)) = self
            .visible_tracks()
            .get(self.main_idx)
            .map(|track| (track.id, track.title.clone()))
        else {
            return;
        };
        self.overlay = Some(Overlay::DeleteTrack {
            track_id,
            title,
            choice: DeleteChoice::Melos,
        });
    }

    fn start_rename(&mut self) {
        match self.view {
            View::Playlists => {
                if let Some(playlist) = self.vm.playlists.get(self.main_idx) {
                    self.input = Some(Input {
                        kind: InputKind::RenamePlaylist(playlist.id),
                        buffer: playlist.name.clone(),
                    });
                }
            }
            View::Library | View::PlaylistTracks => {
                if let Some(track) = self.visible_tracks().get(self.main_idx) {
                    self.input = Some(Input {
                        kind: InputKind::RenameTrack(track.id),
                        buffer: track.title.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    fn activate(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.focus == Focus::Sidebar {
            self.sidebar_idx = self.sidebar_idx.min(SIDEBAR_VIEWS.len() - 1);
            self.set_view(SIDEBAR_VIEWS[self.sidebar_idx]);
            self.focus = Focus::Main;
            if self.view == View::Stats {
                actions.push(Action::RequestStats);
            }
            return actions;
        }
        match self.view {
            View::Library | View::PlaylistTracks => {
                if let Some(track_id) = self.selected_track_id() {
                    actions.push(Action::PlayTrack {
                        track_id,
                        context: self.context_ids(),
                    });
                }
            }
            View::Playlists => {
                if let Some(playlist) = self.vm.playlists.get(self.main_idx) {
                    actions.push(Action::SelectPlaylist {
                        playlist_id: Some(playlist.id),
                    });
                    self.view = View::PlaylistTracks;
                    self.main_idx = 0;
                }
            }
            View::Queue => {
                if !self.vm.queue.is_empty() {
                    actions.push(Action::PlayQueueIndex {
                        index: self.main_idx,
                    });
                }
            }
            View::Downloads => {
                if let Some(result) = self.vm.yt_results.get(self.main_idx) {
                    actions.push(Action::Download {
                        url: result.url.clone(),
                        title: Some(result.title.clone()),
                        uploader: Some(result.uploader.clone()),
                    });
                    self.main_idx = 0;
                }
            }
            View::Stats => {}
        }
        actions
    }

    /// Sidebar navigation (used by the UI when focus is on the sidebar).
    pub fn sidebar_move(&mut self, delta: isize) {
        let next = self.sidebar_idx as isize + delta;
        self.sidebar_idx = next.clamp(0, SIDEBAR_VIEWS.len() as isize - 1) as usize;
        self.set_view(SIDEBAR_VIEWS[self.sidebar_idx]);
    }

    #[allow(dead_code)]
    pub fn sidebar_activate(&mut self) -> Vec<Action> {
        self.activate()
    }

    pub fn sidebar_labels(&self) -> Vec<(String, usize)> {
        vec![
            ("Library".to_string(), self.vm.tracks.len()),
            ("Playlists".to_string(), self.vm.playlists.len()),
            ("Queue".to_string(), self.vm.queue.len()),
            (
                "Downloads".to_string(),
                self.vm
                    .downloads
                    .iter()
                    .filter(|d| !d.state.is_finished())
                    .count(),
            ),
            ("Statistics".to_string(), 0),
        ]
    }

    #[allow(dead_code)]
    pub fn download_label(&self, d: &melos_proto::DownloadRow) -> String {
        match d.state {
            DownloadState::Downloading => format!(
                "{}  {:>3.0}%  {}",
                truncate(&d.title, 34),
                d.percent,
                d.detail
            ),
            state => format!("{}  {}", truncate(&d.title, 34), state.label()),
        }
    }
}

pub fn fuzzy_match(query: &str, text: &str) -> bool {
    let mut haystack = text.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|needle| haystack.any(|c| c == needle))
}

#[allow(dead_code)]
pub fn track_label(t: &TrackRow) -> String {
    format!(
        "{} — {}  {}",
        t.title,
        t.artist,
        format_duration(t.duration_ms)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn track(id: i64, title: &str) -> TrackRow {
        TrackRow {
            id,
            title: title.into(),
            artist: "Artist".into(),
            album: None,
            duration_ms: 60_000,
            missing: false,
        }
    }

    fn app_with_tracks() -> App {
        let vm = ViewModel {
            tracks: vec![track(1, "One"), track(2, "Two"), track(3, "Three")],
            transport: melos_proto::Transport {
                track: Some(track(2, "Two")),
                ..Default::default()
            },
            ..ViewModel::default()
        };
        App::new(
            vm,
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        )
    }

    #[test]
    fn fuzzy_subsequence() {
        assert!(fuzzy_match("dft", "Daft Punk"));
        assert!(fuzzy_match("", "anything"));
        assert!(!fuzzy_match("zz", "Daft Punk"));
    }

    #[test]
    fn download_summary_lengths() {
        let app = App::new(
            ViewModel::default(),
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        );
        let d = melos_proto::DownloadRow {
            id: 1,
            title: "x".repeat(100),
            state: DownloadState::Downloading,
            percent: 42.0,
            detail: "1MiB/s".into(),
            error: None,
        };
        assert!(app.download_label(&d).chars().count() < 80);
    }

    #[test]
    fn library_reveals_the_currently_playing_track() {
        let app = app_with_tracks();
        assert_eq!(app.main_idx, 1);
        assert_eq!(app.selected_track_id(), Some(2));
    }

    #[test]
    fn delete_opens_a_safe_choice_and_removes_from_melos_by_default() {
        let mut app = app_with_tracks();
        assert!(app.on_key(key(KeyCode::Delete)).is_empty());
        assert!(matches!(
            app.overlay,
            Some(Overlay::DeleteTrack {
                track_id: 2,
                choice: DeleteChoice::Melos,
                ..
            })
        ));
        let actions = app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            actions.as_slice(),
            [Action::DeleteTrack {
                track_id: 2,
                delete_file: false
            }]
        ));
        assert!(app.overlay.is_none());
    }

    #[test]
    fn delete_choice_can_select_the_file_and_warns() {
        let mut app = app_with_tracks();
        app.on_key(key(KeyCode::Delete));
        app.on_key(key(KeyCode::Char('j')));
        let actions = app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            actions.as_slice(),
            [Action::DeleteTrack {
                track_id: 2,
                delete_file: true
            }]
        ));
    }

    #[test]
    fn rename_prompt_is_prefilled_and_submits_the_new_name() {
        let vm = ViewModel {
            playlists: vec![melos_proto::PlaylistRow {
                id: 7,
                name: "Old".into(),
                count: 0,
            }],
            ..ViewModel::default()
        };
        let mut app = App::new(
            vm,
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        );
        app.set_view(View::Playlists);
        app.on_key(key(KeyCode::Char('e')));
        let input = app.input.as_ref().expect("rename prompt");
        assert_eq!(input.buffer, "Old");
        let actions = app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            actions.as_slice(),
            [Action::RenamePlaylist {
                playlist_id: 7,
                name
            }] if name == "Old"
        ));
    }

    #[test]
    fn playlist_tracks_keep_d_as_remove_from_playlist() {
        let vm = ViewModel {
            tracks: vec![track(1, "One")],
            selected_playlist: Some(3),
            playlist_tracks: vec![track(1, "One")],
            ..ViewModel::default()
        };
        let mut app = App::new(
            vm,
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        );
        app.view = View::PlaylistTracks;
        let actions = app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(
            actions.as_slice(),
            [Action::RemoveFromPlaylist {
                playlist_id: 3,
                track_id: 1
            }]
        ));
    }

    #[test]
    fn stats_pane_requests_fresh_totals() {
        let mut app = app_with_tracks();
        let actions = app.on_key(key(KeyCode::Char('5')));
        assert_eq!(app.view, View::Stats);
        assert!(matches!(actions.as_slice(), [Action::RequestStats]));
    }

    #[test]
    fn initial_snapshot_art_is_kept_and_decoded_off_the_event_loop() {
        let dir = std::env::temp_dir().join(format!("melos-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cover.png");
        image::RgbImage::from_pixel(2, 2, image::Rgb([214, 168, 95]))
            .save(&path)
            .unwrap();
        let art = path.to_string_lossy().into_owned();

        let mut vm = ViewModel::default();
        vm.transport.art_path = Some(art.clone());
        let mut app = App::new(
            vm,
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        );
        assert_eq!(app.art_path.as_deref(), Some(art.as_str()));
        app.set_picker(Picker::halfblocks());
        assert!(app.art_pending(), "decode is queued off-thread");
        for _ in 0..100 {
            app.poll_art();
            if app.art.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.art.is_some(), "background art decode completed");
        assert!(!app.art_pending(), "decode queue is drained");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
