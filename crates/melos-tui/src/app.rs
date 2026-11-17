//! Client-local UI state: view, cursors, filter, input prompts, overlays.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use melos_proto::{
    Action, DownloadState, ServerMsg, TrackRow, ViewModel, format_duration, truncate,
};

use crate::keys::KeyMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Library,
    Playlists,
    PlaylistTracks,
    Queue,
    Downloads,
}

impl View {
    pub fn title(self) -> &'static str {
        match self {
            View::Library => "Library",
            View::Playlists => "Playlists",
            View::PlaylistTracks => "Playlist",
            View::Queue => "Queue",
            View::Downloads => "Downloads",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Filter,
    Download,
    NewPlaylist,
}

pub struct Input {
    pub kind: InputKind,
    pub buffer: String,
}

#[derive(Debug, Clone)]
pub enum Overlay {
    Help,
    PlaylistPicker { track_id: i64, index: usize },
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
}

const SIDEBAR_VIEWS: [View; 4] = [View::Library, View::Playlists, View::Queue, View::Downloads];

impl App {
    pub fn new(vm: ViewModel, keys: KeyMap) -> Self {
        let daemon_version = vm.daemon_version.clone();
        Self {
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
                self.vm = snapshot;
            }
            ServerMsg::State { snapshot } => {
                self.vm = snapshot;
                if self.view == View::PlaylistTracks && self.vm.selected_playlist.is_none() {
                    self.view = View::Playlists;
                    self.main_idx = 0;
                }
                if !self.daemon_lost {
                    self.status_override = None;
                }
            }
            ServerMsg::Transport { transport } => {
                self.vm.transport = transport;
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

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.main_idx = 0;
        self.filter.clear();
        if let Some(input) = &self.input
            && input.kind == InputKind::Filter
        {
            self.input = None;
        }
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
                    match input.kind {
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
                } else {
                    self.move_cursor(1);
                }
            }
            "up" => {
                if self.focus == Focus::Sidebar {
                    self.sidebar_move(-1);
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
                _ => {}
            },
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

    fn activate(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.focus == Focus::Sidebar {
            self.sidebar_idx = self.sidebar_idx.min(SIDEBAR_VIEWS.len() - 1);
            self.set_view(SIDEBAR_VIEWS[self.sidebar_idx]);
            self.focus = Focus::Main;
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
}
