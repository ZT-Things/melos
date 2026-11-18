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
    Lyrics,
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
            View::Lyrics => "Lyrics",
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
    LyricsPaste(i64),
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
    art_aspect: Option<f32>,
    art_rx: Option<Receiver<(String, StatefulProtocol, f32)>>,
    reveal_after_state: bool,
    pub lyrics_scroll: usize,
    pub lyrics_selection: Option<usize>,
    /// Rows available in the lyrics body, updated while drawing.
    pub lyrics_page: usize,
    pub visual_anchor: Option<usize>,
    editor_request: Option<i64>,
}

const SIDEBAR_VIEWS: [View; 6] = [
    View::Library,
    View::Playlists,
    View::Queue,
    View::Downloads,
    View::Lyrics,
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
            art_aspect: None,
            art_rx: None,
            reveal_after_state: false,
            lyrics_scroll: 0,
            lyrics_selection: None,
            lyrics_page: 10,
            visual_anchor: None,
            editor_request: None,
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
        self.art_aspect = None;
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
                let aspect = image.height() as f32 / image.width().max(1) as f32;
                let protocol = picker.new_resize_protocol(image);
                let _ = tx.send((path, protocol, aspect));
            }
        });
    }

    pub fn art_pending(&self) -> bool {
        self.art_rx.is_some()
    }

    /// Height/width ratio of the decoded cover, once available.
    pub fn art_aspect(&self) -> Option<f32> {
        self.art_aspect
    }

    /// Apply a finished background decode. Called from the draw path so the
    /// event loop never waits on image decoding.
    pub fn poll_art(&mut self) {
        let Some(rx) = self.art_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok((path, protocol, aspect)) => {
                if self.art_path.as_deref() == Some(path.as_str()) {
                    self.art = Some(protocol);
                    self.art_aspect = Some(aspect);
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
                if self.reveal_after_state {
                    self.reveal_after_state = false;
                    self.reveal_current();
                }
                if self.visual_anchor.is_some() {
                    if self.can_reorder() {
                        self.clamp_cursor();
                        let len = self.main_len();
                        self.visual_anchor = self.visual_anchor.map(|anchor| anchor.min(len - 1));
                    } else {
                        self.visual_anchor = None;
                    }
                }
                if !self.daemon_lost {
                    self.status_override = None;
                }
            }
            ServerMsg::Transport { transport } => {
                let previous = self.vm.transport.track.as_ref().map(|track| track.id);
                let current = transport.track.as_ref().map(|track| track.id);
                let art = transport.art_path.clone();
                self.vm.transport = transport;
                self.set_art_path(art);
                if previous != current {
                    self.lyrics_scroll = 0;
                    self.lyrics_selection = None;
                }
            }
            ServerMsg::Lyrics {
                track_id,
                lyrics,
                loading,
                error,
            } => {
                if self.vm.transport.track.as_ref().map(|track| track.id) == Some(track_id) {
                    self.vm.lyrics = lyrics;
                    self.vm.lyrics_loading = loading;
                    self.vm.lyrics_error = error;
                    if !loading {
                        self.lyrics_scroll = 0;
                        self.lyrics_selection = None;
                    }
                }
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
            View::Lyrics | View::Stats => 0,
        }
    }

    pub fn visual_range(&self) -> Option<(usize, usize)> {
        let anchor = self.visual_anchor?;
        let len = self.main_len();
        if len == 0 {
            return None;
        }
        let cursor = self.main_idx.min(len - 1);
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    fn can_reorder(&self) -> bool {
        self.focus == Focus::Main
            && matches!(
                self.view,
                View::Library | View::PlaylistTracks | View::Queue
            )
            && self.filter.is_empty()
            && self.main_len() > 0
    }

    fn toggle_visual(&mut self) {
        if self.visual_anchor.is_some() {
            self.visual_anchor = None;
        } else if self.can_reorder() {
            self.visual_anchor = Some(self.main_idx);
        }
    }

    fn move_visual(&mut self, delta: i8) -> Option<Action> {
        if !self.can_reorder() {
            return None;
        }
        let (start, end) = self.visual_range()?;
        let len = end - start + 1;
        let count = self.main_len();
        if (delta == 1 && end + 1 >= count) || (delta == -1 && start == 0) {
            return None;
        }
        let action = match self.view {
            View::Library => Action::MoveLibrary { start, len, delta },
            View::PlaylistTracks => Action::MovePlaylistTracks {
                playlist_id: self.vm.selected_playlist?,
                start,
                len,
                delta,
            },
            View::Queue => Action::MoveQueue { start, len, delta },
            _ => return None,
        };
        let shift = delta as isize;
        self.main_idx = (self.main_idx as isize + shift).clamp(0, count as isize - 1) as usize;
        self.visual_anchor = self
            .visual_anchor
            .map(|anchor| (anchor as isize + shift).clamp(0, count as isize - 1) as usize);
        Some(action)
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
        let Some(current) = self.vm.transport.track.as_ref().map(|t| t.id) else {
            return;
        };
        match self.view {
            View::Library | View::PlaylistTracks => {
                if let Some(index) = self
                    .visible_tracks()
                    .iter()
                    .position(|track| track.id == current)
                {
                    self.main_idx = index;
                }
            }
            View::Queue => {
                if let Some(index) = self
                    .vm
                    .queue_pos
                    .filter(|index| *index < self.vm.queue.len())
                {
                    self.main_idx = index;
                } else if let Some(index) =
                    self.vm.queue.iter().position(|track| track.id == current)
                {
                    self.main_idx = index;
                }
            }
            _ => {}
        }
    }

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.main_idx = 0;
        self.lyrics_scroll = 0;
        self.lyrics_selection = None;
        self.visual_anchor = None;
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

    fn lyric_seek_timestamp(&self) -> Option<u64> {
        let lyrics = self.vm.lyrics.as_ref()?;
        let lines = melos_proto::lyrics::parse_lrc(lyrics.synced.as_deref()?);
        let index = self.lyrics_selection.or_else(|| {
            melos_proto::lyrics::current_line(
                &lines,
                self.vm.transport.position_ms,
                lyrics.offset_ms,
            )
        })?;
        lines
            .get(index.min(lines.len().saturating_sub(1)))
            .map(|line| line.timestamp_ms)
    }

    fn lyrics_half_page(&self) -> isize {
        (self.lyrics_page.max(2) / 2) as isize
    }

    fn scroll_lyrics(&mut self, delta: isize) {
        let Some(lyrics) = self.vm.lyrics.as_ref() else {
            return;
        };
        let lines = lyrics
            .synced
            .as_deref()
            .map(melos_proto::lyrics::parse_lrc)
            .unwrap_or_default();
        if lines.is_empty() {
            let count = lyrics.plain.lines().count().saturating_sub(1);
            self.lyrics_scroll =
                (self.lyrics_scroll as isize + delta).clamp(0, count as isize) as usize;
            return;
        }
        let current = melos_proto::lyrics::current_line(
            &lines,
            self.vm.transport.position_ms,
            lyrics.offset_ms,
        )
        .unwrap_or(0) as isize;
        let base = self
            .lyrics_selection
            .map(|index| index as isize)
            .unwrap_or(current);
        self.lyrics_selection =
            Some((base + delta).clamp(0, lines.len().saturating_sub(1) as isize) as usize);
    }

    pub fn on_paste(&mut self, text: &str) {
        if let Some(input) = &mut self.input {
            input.buffer.push_str(text);
            if input.kind == InputKind::Filter {
                self.filter = input.buffer.clone();
                self.main_idx = 0;
            }
        }
    }

    pub fn take_editor_request(&mut self) -> Option<(i64, String)> {
        let track_id = self.editor_request.take()?;
        let text = self
            .vm
            .lyrics
            .as_ref()
            .filter(|lyrics| lyrics.track_id == track_id)
            .map(|lyrics| {
                lyrics
                    .synced
                    .clone()
                    .unwrap_or_else(|| lyrics.plain.clone())
            })
            .unwrap_or_default();
        Some((track_id, text))
    }

    fn open_lyrics_paste(&mut self) {
        if let Some(track_id) = self.vm.transport.track.as_ref().map(|t| t.id) {
            self.input = Some(Input {
                kind: InputKind::LyricsPaste(track_id),
                buffer: String::new(),
            });
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return Vec::new();
        }
        let mut actions = Vec::new();

        if let Some(mut input) = self.input.take() {
            let lyrics_paste = matches!(input.kind, InputKind::LyricsPaste(_));
            match key.code {
                KeyCode::Esc => {
                    if input.kind == InputKind::Filter {
                        self.filter.clear();
                    }
                }
                KeyCode::Enter if lyrics_paste => {
                    input.buffer.push('\n');
                }
                KeyCode::Char('s')
                    if lyrics_paste && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let InputKind::LyricsPaste(track_id) = input.kind {
                        let text = input.buffer.trim().to_string();
                        if !text.is_empty() {
                            actions.push(Action::SaveLyrics { track_id, text });
                        }
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
                        InputKind::LyricsPaste(_) => {}
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
            // Keep the prompt open unless it was submitted, saved, or cancelled.
            let saved = lyrics_paste
                && key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL);
            let submitted = key.code == KeyCode::Enter && !lyrics_paste;
            let still_open = !(matches!(key.code, KeyCode::Esc) || submitted || saved);
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

        if self.view == View::Lyrics {
            // Lowercase aliases are scoped to the lyrics pane so `p` does not
            // stop meaning "previous track" everywhere else.
            let plain_mods = !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT);
            match key.code {
                KeyCode::Char('p') if plain_mods => {
                    self.open_lyrics_paste();
                    return actions;
                }
                KeyCode::Char('e') if plain_mods => {
                    if let Some(track_id) = self.vm.transport.track.as_ref().map(|t| t.id) {
                        self.editor_request = Some(track_id);
                    }
                    return actions;
                }
                KeyCode::Char('f') if plain_mods => {
                    actions.push(Action::RequestLyrics { force: true });
                    return actions;
                }
                _ => {}
            }
        }

        let Some(action) = self.keys.action(key).map(str::to_string) else {
            return actions;
        };

        if self.view == View::Lyrics && self.focus == Focus::Main {
            match action.as_str() {
                "down" => {
                    self.scroll_lyrics(1);
                    return actions;
                }
                "up" => {
                    self.scroll_lyrics(-1);
                    return actions;
                }
                "half_down" => {
                    self.scroll_lyrics(self.lyrics_half_page());
                    return actions;
                }
                "half_up" => {
                    self.scroll_lyrics(-self.lyrics_half_page());
                    return actions;
                }
                "page_down" => {
                    self.scroll_lyrics(self.lyrics_page.max(1) as isize);
                    return actions;
                }
                "page_up" => {
                    self.scroll_lyrics(-(self.lyrics_page.max(1) as isize));
                    return actions;
                }
                "confirm" => {
                    if let Some(timestamp) = self.lyric_seek_timestamp() {
                        actions.push(Action::SeekTo {
                            seconds: timestamp / 1000,
                        });
                        self.lyrics_selection = None;
                    }
                    return actions;
                }
                "cancel" => {
                    self.lyrics_selection = None;
                    return actions;
                }
                _ => {}
            }
        }

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
                    if self.view == View::Lyrics {
                        actions.push(Action::RequestLyrics { force: false });
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
                    if self.view == View::Lyrics {
                        actions.push(Action::RequestLyrics { force: false });
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
                self.visual_anchor = None;
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
            "pane_lyrics" => {
                self.sidebar_idx = 4;
                self.set_view(View::Lyrics);
                self.focus = Focus::Main;
                actions.push(Action::RequestLyrics { force: false });
            }
            "pane_stats" => {
                self.sidebar_idx = 5;
                self.set_view(View::Stats);
                self.focus = Focus::Main;
                actions.push(Action::RequestStats);
            }
            "lyrics_paste" => {
                if self.view == View::Lyrics {
                    self.open_lyrics_paste();
                }
            }
            "lyrics_edit" => {
                if self.view == View::Lyrics
                    && let Some(track_id) = self.vm.transport.track.as_ref().map(|t| t.id)
                {
                    self.editor_request = Some(track_id);
                }
            }
            "lyrics_fetch" => {
                if self.view == View::Lyrics {
                    actions.push(Action::RequestLyrics { force: true });
                }
            }
            "lyrics_offset_back" => {
                if self.view == View::Lyrics
                    && let Some(lyrics) = &self.vm.lyrics
                {
                    actions.push(Action::SetLyricsOffset {
                        track_id: lyrics.track_id,
                        offset_ms: (lyrics.offset_ms - 500).clamp(-60_000, 60_000),
                    });
                }
            }
            "lyrics_offset_fwd" => {
                if self.view == View::Lyrics
                    && let Some(lyrics) = &self.vm.lyrics
                {
                    actions.push(Action::SetLyricsOffset {
                        track_id: lyrics.track_id,
                        offset_ms: (lyrics.offset_ms + 500).clamp(-60_000, 60_000),
                    });
                }
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
            "visual" => self.toggle_visual(),
            "move_down" => {
                if let Some(action) = self.move_visual(1) {
                    actions.push(action);
                }
            }
            "move_up" => {
                if let Some(action) = self.move_visual(-1) {
                    actions.push(action);
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
                if self.visual_anchor.is_some() {
                    self.visual_anchor = None;
                } else if !self.filter.is_empty() {
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
            if self.view == View::Lyrics {
                actions.push(Action::RequestLyrics { force: false });
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
                    self.filter.clear();
                    if let Some(input) = &self.input
                        && input.kind == InputKind::Filter
                    {
                        self.input = None;
                    }
                    self.reveal_after_state = self.vm.transport.track.is_some();
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
            View::Lyrics | View::Stats => {}
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
            ("Lyrics".to_string(), 0),
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

    fn key_mods(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
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
    fn visual_selection_moves_the_library_block_and_follows_it() {
        let mut app = app_with_tracks();
        app.main_idx = 0;
        app.on_key(key(KeyCode::Char('V')));
        assert_eq!(app.visual_range(), Some((0, 0)));

        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.visual_range(), Some((0, 1)));

        let actions = app.on_key(key(KeyCode::Char('J')));
        assert_eq!(
            actions,
            vec![Action::MoveLibrary {
                start: 0,
                len: 2,
                delta: 1
            }]
        );
        assert_eq!(app.visual_range(), Some((1, 2)));

        assert!(app.on_key(key(KeyCode::Char('J'))).is_empty());
        assert!(app.on_key(key(KeyCode::Char('K'))).len() == 1);

        app.on_key(key(KeyCode::Char('V')));
        assert!(app.visual_range().is_none());
    }

    #[test]
    fn visual_reorder_works_in_playlists_and_the_queue() {
        let mut vm = ViewModel {
            playlists: vec![melos_proto::PlaylistRow {
                id: 9,
                name: "Mix".into(),
                count: 3,
            }],
            playlist_tracks: vec![track(1, "One"), track(2, "Two"), track(3, "Three")],
            selected_playlist: Some(9),
            ..ViewModel::default()
        };
        vm.queue = vm.playlist_tracks.clone();
        let mut app = App::new(
            vm,
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        );

        app.view = View::PlaylistTracks;
        app.on_key(key(KeyCode::Char('V')));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('J'))),
            vec![Action::MovePlaylistTracks {
                playlist_id: 9,
                start: 0,
                len: 2,
                delta: 1
            }]
        );

        app.set_view(View::Queue);
        app.main_idx = 1;
        app.on_key(key(KeyCode::Char('V')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('K'))),
            vec![Action::MoveQueue {
                start: 1,
                len: 1,
                delta: -1
            }]
        );
    }

    #[test]
    fn visual_mode_is_blocked_by_a_filter_and_escape_exits() {
        let mut app = app_with_tracks();
        app.filter = "o".into();
        app.on_key(key(KeyCode::Char('V')));
        assert!(app.visual_range().is_none());

        app.filter.clear();
        app.on_key(key(KeyCode::Char('V')));
        assert!(app.visual_range().is_some());
        app.on_key(key(KeyCode::Esc));
        assert!(app.visual_range().is_none());
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
    fn queue_reveals_the_currently_playing_track() {
        let mut app = app_with_tracks();
        app.vm.queue = vec![track(1, "One"), track(2, "Two"), track(3, "Three")];
        app.vm.queue_pos = Some(2);
        app.set_view(View::Queue);
        assert_eq!(app.main_idx, 2);
    }

    #[test]
    fn playlist_reveals_the_currently_playing_track() {
        let mut app = app_with_tracks();
        app.vm.playlist_tracks = vec![track(1, "One"), track(2, "Two"), track(3, "Three")];
        app.set_view(View::PlaylistTracks);
        assert_eq!(app.main_idx, 1);
    }

    #[test]
    fn opening_a_playlist_reveals_the_current_track_after_loading_it() {
        let mut app = app_with_tracks();
        app.vm.playlists = vec![melos_proto::PlaylistRow {
            id: 3,
            name: "Mix".into(),
            count: 3,
        }];
        app.set_view(View::Playlists);
        let actions = app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            actions.as_slice(),
            [Action::SelectPlaylist {
                playlist_id: Some(3)
            }]
        ));
        assert_eq!(app.view, View::PlaylistTracks);
        assert_eq!(app.main_idx, 0);

        let mut snapshot = app.vm.clone();
        snapshot.selected_playlist = Some(3);
        snapshot.playlist_tracks = vec![track(1, "One"), track(2, "Two"), track(3, "Three")];
        app.apply(ServerMsg::State { snapshot });
        assert_eq!(app.main_idx, 1);
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
        let actions = app.on_key(key(KeyCode::Char('6')));
        assert_eq!(app.view, View::Stats);
        assert!(matches!(actions.as_slice(), [Action::RequestStats]));
    }

    #[test]
    fn lyrics_pane_requests_lyrics() {
        let mut app = app_with_tracks();
        let actions = app.on_key(key(KeyCode::Char('5')));
        assert_eq!(app.view, View::Lyrics);
        assert!(matches!(
            actions.as_slice(),
            [Action::RequestLyrics { force: false }]
        ));
    }

    fn app_with_lyrics() -> App {
        let mut app = app_with_tracks();
        app.sidebar_idx = 4;
        app.view = View::Lyrics;
        app.vm.transport.position_ms = 6_000;
        app.vm.lyrics = Some(melos_proto::LyricsView {
            track_id: 2,
            plain: "one\ntwo".into(),
            synced: Some("[00:05.00]one\n[00:10.00]two".into()),
            source: "manual".into(),
            offset_ms: 0,
            updated_at: 0,
        });
        app
    }

    #[test]
    fn pasting_lyrics_and_ctrl_s_saves_them() {
        let mut app = app_with_lyrics();
        app.on_key(key(KeyCode::Char('P')));
        assert!(matches!(
            app.input.as_ref().map(|i| &i.kind),
            Some(InputKind::LyricsPaste(2))
        ));
        app.on_paste("[00:01.00]Hello");
        let actions = app.on_key(key_mods(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(
            actions.as_slice(),
            [Action::SaveLyrics { track_id: 2, text }] if text == "[00:01.00]Hello"
        ));
        assert!(app.input.is_none());
    }

    #[test]
    fn lyrics_offset_and_seek_follow_the_synced_line() {
        let mut app = app_with_lyrics();
        let actions = app.on_key(key(KeyCode::Char(']')));
        assert!(matches!(
            actions.as_slice(),
            [Action::SetLyricsOffset {
                track_id: 2,
                offset_ms: 500
            }]
        ));
        app.vm.lyrics.as_mut().unwrap().offset_ms = 60_000;
        let actions = app.on_key(key(KeyCode::Char(']')));
        assert!(matches!(
            actions.as_slice(),
            [Action::SetLyricsOffset {
                track_id: 2,
                offset_ms: 60_000
            }]
        ));
        app.vm.lyrics.as_mut().unwrap().offset_ms = 0;
        let actions = app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            actions.as_slice(),
            [Action::SeekTo { seconds: 5 }]
        ));
    }

    #[test]
    fn editor_handoff_uses_the_current_synced_text() {
        let mut app = app_with_lyrics();
        app.on_key(key(KeyCode::Char('E')));
        let (track_id, text) = app.take_editor_request().expect("editor request");
        assert_eq!(track_id, 2);
        assert!(text.starts_with("[00:05.00]one"));
    }

    #[test]
    fn lowercase_lyrics_keys_work_without_shift() {
        let mut app = app_with_lyrics();
        app.on_key(key(KeyCode::Char('p')));
        assert!(matches!(
            app.input.as_ref().map(|i| &i.kind),
            Some(InputKind::LyricsPaste(2))
        ));
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Char('e')));
        assert!(app.take_editor_request().is_some());
        let actions = app.on_key(key(KeyCode::Char('f')));
        assert!(matches!(
            actions.as_slice(),
            [Action::RequestLyrics { force: true }]
        ));
    }

    #[test]
    fn page_keys_scroll_plain_lyrics() {
        let mut app = app_with_lyrics();
        let lyrics = app.vm.lyrics.as_mut().unwrap();
        lyrics.synced = None;
        lyrics.plain = (0..30)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.lyrics_scroll, 10);
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.lyrics_scroll, 20);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.lyrics_scroll, 10);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.lyrics_scroll, 0);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.lyrics_scroll, 0);
    }

    #[test]
    fn half_page_keys_scroll_by_half_the_lyrics_viewport() {
        let mut app = app_with_lyrics();
        let lyrics = app.vm.lyrics.as_mut().unwrap();
        lyrics.synced = None;
        lyrics.plain = (0..30)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.lyrics_page = 20;

        app.on_key(key_mods(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.lyrics_scroll, 10);
        app.on_key(key_mods(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.lyrics_scroll, 20);
        app.on_key(key_mods(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.lyrics_scroll, 10);
        app.on_key(key_mods(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.lyrics_scroll, 0);
    }

    #[test]
    fn half_page_keys_move_the_synced_selection() {
        let mut app = app_with_lyrics();
        app.vm.lyrics.as_mut().unwrap().synced = Some(
            (0..30)
                .map(|n| format!("[00:{n:02}.00]line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.lyrics_page = 20;

        app.on_key(key_mods(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.lyrics_selection, Some(16));
        app.on_key(key_mods(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.lyrics_selection, Some(6));
    }

    #[test]
    fn synced_lyrics_select_a_line_and_seek_to_it() {
        let mut app = app_with_lyrics();
        assert_eq!(app.lyrics_selection, None);
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.lyrics_selection, Some(1));
        let actions = app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            actions.as_slice(),
            [Action::SeekTo { seconds: 10 }]
        ));
        assert_eq!(app.lyrics_selection, None);
    }

    #[test]
    fn escape_returns_synced_lyrics_to_playback_follow() {
        let mut app = app_with_lyrics();
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.lyrics_selection, Some(1));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.lyrics_selection, None);
    }

    #[test]
    fn page_keys_move_the_synced_selection() {
        let mut app = app_with_lyrics();
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.lyrics_selection, Some(1));
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.lyrics_selection, Some(1));
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.lyrics_selection, Some(0));
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.lyrics_selection, Some(0));
    }

    #[test]
    fn p_still_means_previous_outside_the_lyrics_view() {
        let mut app = app_with_tracks();
        app.view = View::Library;
        let actions = app.on_key(key(KeyCode::Char('p')));
        assert!(matches!(actions.as_slice(), [Action::Prev]));
    }

    #[test]
    fn sidebar_navigation_still_works_from_the_lyrics_view() {
        let mut app = app_with_lyrics();
        app.focus = Focus::Sidebar;
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.view, View::Stats);
        assert_eq!(app.lyrics_scroll, 0);
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
        assert_eq!(app.art_aspect(), Some(1.0));
        assert!(!app.art_pending(), "decode queue is drained");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
