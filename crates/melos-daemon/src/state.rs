//! Authoritative daemon state and the action reducer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use melos_proto::{
    Action, DownloadRow, DownloadState, PlaylistRow, Repeat, ServerMsg, TrackRow, Transport,
    ViewModel, YtResultRow, config::Config, paths,
};
use mpris_server::Server;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::art;
use crate::db::Db;
use crate::download;
use crate::mpris::{self, Info, Mpris, TrackInfo};
use crate::mpv::MpvEvent;
use crate::player::Player;

pub enum Internal {
    Action(Action),
    Mpv(MpvEvent),
    Download(download::Event),
    Client(mpsc::UnboundedSender<ServerMsg>),
    Mpris(Arc<Server<Mpris>>, Arc<Mutex<Info>>),
    Artwork { track_id: i64, path: Option<String> },
    Note(String),
    Tick,
}

pub enum Control {
    Continue,
    Shutdown,
}

pub struct Daemon {
    cfg: Arc<Config>,
    db: Db,
    player: Player,
    tracks: Vec<TrackRow>,
    playlists: Vec<PlaylistRow>,
    playlist_tracks: Vec<TrackRow>,
    selected_playlist: Option<i64>,
    downloads: Vec<DownloadRow>,
    jobs: HashMap<u64, JoinHandle<()>>,
    next_job: u64,
    yt_results: Vec<YtResultRow>,
    clients: Vec<mpsc::UnboundedSender<ServerMsg>>,
    tx: mpsc::UnboundedSender<Internal>,
    sem: Arc<Semaphore>,
    status: String,
    transport_dirty: bool,
    last_persist: Instant,
    mpris: Option<Arc<Server<Mpris>>>,
    mpris_info: Option<Arc<Mutex<Info>>>,
    listening: ListenTracker,
    art_path: Option<String>,
    art_track: Option<i64>,
}

impl Daemon {
    pub fn new(
        cfg: Arc<Config>,
        db: Db,
        player: Player,
        tx: mpsc::UnboundedSender<Internal>,
    ) -> Self {
        let persisted = load_state();
        let mut player = player;
        if let Some(p) = &persisted {
            player.volume = p.volume.unwrap_or(70).clamp(0, 100);
            player.muted = p.muted;
            player.shuffle = p.shuffle;
            player.repeat = p.repeat;
            player.queue = p.queue.clone();
            player.pos = p.pos.filter(|i| *i < player.queue.len());
            player.position_ms = p.position_ms;
            player.reseed_shuffle();
        } else {
            player.volume = 70;
        }

        let tracks = db.rows().unwrap_or_default();
        let playlists = db.playlists().unwrap_or_default();
        let selected_playlist = persisted
            .as_ref()
            .and_then(|p| p.selected_playlist)
            .filter(|id| playlists.iter().any(|p| p.id == *id));
        let playlist_tracks = selected_playlist
            .and_then(|id| db.playlist_tracks(id).ok())
            .unwrap_or_default();

        let status = if player.has_mpv() {
            "ready".to_string()
        } else {
            "mpv is not available — playback disabled".to_string()
        };

        Self {
            cfg,
            db,
            player,
            tracks,
            playlists,
            playlist_tracks,
            selected_playlist,
            downloads: Vec::new(),
            jobs: HashMap::new(),
            next_job: 1,
            yt_results: Vec::new(),
            clients: Vec::new(),
            tx,
            sem: Arc::new(Semaphore::new(1)), // replaced in main with configured size
            status,
            transport_dirty: false,
            last_persist: Instant::now(),
            mpris: None,
            mpris_info: None,
            listening: ListenTracker::new(),
            art_path: None,
            art_track: None,
        }
    }

    fn listen_tick(&mut self) {
        let active = if self.player.playing && self.player.is_loaded() {
            self.player
                .current_id()
                .and_then(|id| self.db.track(id).ok().flatten())
                .map(|(row, _)| (row.id, row.title, row.artist))
        } else {
            None
        };
        if let Some(entry) = self.listening.advance(Instant::now(), active)
            && let Err(e) =
                self.db
                    .add_listening(entry.track_id, &entry.title, &entry.artist, entry.ms)
        {
            tracing::warn!("could not record listening time: {e}");
        }
    }

    fn flush_listening(&mut self) {
        if let Some(entry) = self.listening.take()
            && let Err(e) =
                self.db
                    .add_listening(entry.track_id, &entry.title, &entry.artist, entry.ms)
        {
            tracing::warn!("could not record listening time: {e}");
        }
    }

    pub fn set_mpris(&mut self, server: Arc<Server<Mpris>>, info: Arc<Mutex<Info>>) {
        self.mpris = Some(server);
        self.mpris_info = Some(info);
        self.sync_mpris();
    }

    pub fn set_semaphore(&mut self, sem: Arc<Semaphore>) {
        self.sem = sem;
    }

    pub async fn handle(&mut self, internal: Internal) -> Control {
        match internal {
            Internal::Action(action) => {
                if matches!(action, Action::Shutdown) {
                    return Control::Shutdown;
                }
                let seeked = matches!(action, Action::SeekBy { .. } | Action::SeekTo { .. });
                if let Err(e) = self.apply(action) {
                    self.status = e.to_string();
                }
                self.persist();
                self.broadcast_state_seek(seeked.then_some(self.player.position_ms));
                self.transport_dirty = false;
            }
            Internal::Mpv(event) => {
                if let Err(e) = self.on_mpv(event) {
                    self.status = e.to_string();
                    self.broadcast_state();
                }
                self.transport_dirty = true;
            }
            Internal::Download(event) => {
                self.on_download(event);
                self.broadcast_state();
            }
            Internal::Client(tx) => {
                let welcome = ServerMsg::Welcome {
                    protocol: melos_proto::PROTOCOL_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").into(),
                    snapshot: self.snapshot(),
                };
                if tx.send(welcome).is_ok() {
                    self.clients.push(tx);
                }
            }
            Internal::Mpris(server, info) => {
                self.set_mpris(server, info);
            }
            Internal::Artwork { track_id, path } => {
                if self.player.current_id() == Some(track_id) {
                    self.art_path = path;
                    self.art_track = Some(track_id);
                    self.transport_dirty = true;
                }
            }
            Internal::Note(note) => {
                self.status = note;
                self.broadcast_state();
            }
            Internal::Tick => {
                self.listen_tick();
                self.sync_mpris();
                if self.transport_dirty && !self.clients.is_empty() {
                    let transport = self.transport();
                    self.send_all(ServerMsg::Transport { transport });
                    self.transport_dirty = false;
                }
                if self.last_persist.elapsed() > Duration::from_secs(15) {
                    self.persist();
                    self.last_persist = Instant::now();
                }
                self.jobs.retain(|_, handle| !handle.is_finished());
            }
        }
        Control::Continue
    }

    fn apply(&mut self, action: Action) -> Result<()> {
        match action {
            Action::PlayTrack { track_id, context } => {
                self.player.play_track(track_id, &context, &self.db)?;
                self.status = self.now_playing_status();
            }
            Action::PlayQueueIndex { index } => {
                self.player.play_queue_index(index, &self.db)?;
                self.status = self.now_playing_status();
            }
            Action::PauseToggle => {
                if !self.player.is_active() {
                    self.start_current()?;
                } else {
                    self.player.toggle_pause()?;
                }
                self.status = if self.player.playing {
                    self.now_playing_status()
                } else {
                    "paused".into()
                };
            }
            Action::Stop => {
                self.player.stop();
                self.status = "stopped".into();
            }
            Action::SetPaused { paused } => {
                if !self.player.is_active() {
                    if paused {
                        return Ok(());
                    }
                    self.start_current()?;
                } else {
                    self.player.set_playing(!paused);
                }
                self.status = if self.player.playing {
                    self.now_playing_status()
                } else {
                    "paused".into()
                };
            }
            Action::Next => match self.player.next(&self.db, true) {
                Ok(()) => self.status = self.now_playing_status(),
                Err(e) if e.to_string() == "end of queue" => self.status = "end of queue".into(),
                Err(e) => return Err(e),
            },
            Action::Prev => {
                self.player.prev(&self.db, true)?;
                self.status = if self.player.playing {
                    self.now_playing_status()
                } else {
                    "start of queue".into()
                };
            }
            Action::SeekBy { seconds } => {
                if !self.player.is_active() {
                    anyhow::bail!("nothing is playing");
                }
                self.player.seek_relative(seconds);
            }
            Action::SeekTo { seconds } => {
                if !self.player.is_active() {
                    anyhow::bail!("nothing is playing");
                }
                self.player.seek_absolute(seconds);
            }
            Action::SetVolume { volume } => {
                self.player.set_volume(volume);
                self.status = format!("volume {}%", self.player.volume);
            }
            Action::VolumeDelta { delta } => {
                self.player.volume_delta(delta);
                self.status = format!("volume {}%", self.player.volume);
            }
            Action::MuteToggle => {
                self.player.toggle_mute();
                self.status = if self.player.muted {
                    "muted".into()
                } else {
                    format!("volume {}%", self.player.volume)
                };
            }
            Action::ShuffleToggle => {
                self.player.toggle_shuffle();
                self.status = if self.player.shuffle {
                    "shuffle on".into()
                } else {
                    "shuffle off".into()
                };
            }
            Action::RepeatCycle => {
                self.player.cycle_repeat();
                self.status = format!("repeat {}", self.player.repeat.label());
            }
            Action::SetShuffle { on } => {
                self.player.set_shuffle(on);
                self.status = if self.player.shuffle {
                    "shuffle on".into()
                } else {
                    "shuffle off".into()
                };
            }
            Action::SetRepeat { mode } => {
                self.player.set_repeat(mode);
                self.status = format!("repeat {}", self.player.repeat.label());
            }
            Action::QueueTrack { track_id } => {
                let Some((row, _)) = self.db.track(track_id)? else {
                    anyhow::bail!("track is no longer in the library");
                };
                self.player.queue_add(track_id);
                self.status = format!("queued {}", row.title);
            }
            Action::QueueRemove { index } => {
                self.player.queue_remove(index);
                self.status = "removed from queue".into();
            }
            Action::QueueClear => {
                self.player.queue_clear();
                self.status = "queue cleared".into();
            }
            Action::LibraryRefresh => {
                self.refresh_library()?;
                self.status = format!("{} tracks in library", self.tracks.len());
            }
            Action::DeleteTrack {
                track_id,
                delete_file,
            } => {
                let Some((row, path)) = self.db.track(track_id)? else {
                    anyhow::bail!("track is no longer in the library");
                };
                if delete_file {
                    let music = std::fs::canonicalize(self.cfg.music_dir_path())
                        .context("music directory is not available")?;
                    let target = std::fs::canonicalize(&path)
                        .with_context(|| format!("audio file not found: {path}"))?;
                    if !file_within(&music, &target) {
                        anyhow::bail!("refusing to delete a file outside the music directory");
                    }
                    std::fs::remove_file(&target)
                        .with_context(|| format!("could not delete {}", target.display()))?;
                }
                let was_current = self.player.current_id() == Some(track_id);
                self.player.remove_track(track_id);
                self.db.delete_track(track_id)?;
                crate::art::remove(track_id);
                self.refresh_library()?;
                self.refresh_playlists()?;
                self.refresh_playlist_tracks()?;
                self.status = if was_current {
                    format!("removed {} and stopped playback", row.title)
                } else if delete_file {
                    format!("deleted {}", row.title)
                } else {
                    format!("removed {} from melos", row.title)
                };
            }
            Action::RenameTrack { track_id, title } => {
                let title = title.trim();
                if title.is_empty() {
                    anyhow::bail!("track name cannot be empty");
                }
                let Some((row, _)) = self.db.track(track_id)? else {
                    anyhow::bail!("track is no longer in the library");
                };
                self.db.rename_track(track_id, title)?;
                self.refresh_library()?;
                self.refresh_playlist_tracks()?;
                self.status = if self.player.current_id() == Some(track_id) {
                    format!("renamed {} to {title}", row.title)
                } else {
                    format!("renamed to {title}")
                };
            }
            Action::RenamePlaylist { playlist_id, name } => {
                let name = name.trim();
                if name.is_empty() {
                    anyhow::bail!("playlist name cannot be empty");
                }
                self.db.rename_playlist(playlist_id, name)?;
                self.refresh_playlists()?;
                self.status = format!("renamed playlist to {name}");
            }
            Action::RequestStats => {
                self.listen_tick();
                self.flush_listening();
                let stats = self.db.stats()?;
                self.status = format!(
                    "{} tracked listening time",
                    melos_proto::format_duration(stats.total_ms)
                );
            }
            Action::CreatePlaylist { name } => {
                let name = name.trim();
                if name.is_empty() {
                    anyhow::bail!("playlist name cannot be empty");
                }
                let id = self.db.create_playlist(name)?;
                self.selected_playlist = Some(id);
                self.refresh_playlists()?;
                self.refresh_playlist_tracks()?;
                self.status = format!("created playlist {name}");
            }
            Action::DeletePlaylist { playlist_id } => {
                self.db.delete_playlist(playlist_id)?;
                if self.selected_playlist == Some(playlist_id) {
                    self.selected_playlist = None;
                    self.playlist_tracks.clear();
                }
                self.refresh_playlists()?;
                self.status = "playlist deleted".into();
            }
            Action::SelectPlaylist { playlist_id } => {
                self.selected_playlist = playlist_id;
                self.refresh_playlist_tracks()?;
            }
            Action::AddToPlaylist {
                playlist_id,
                track_id,
            } => {
                self.db.add_to_playlist(playlist_id, track_id)?;
                self.refresh_playlists()?;
                if self.selected_playlist == Some(playlist_id) {
                    self.refresh_playlist_tracks()?;
                }
                self.status = "added to playlist".into();
            }
            Action::RemoveFromPlaylist {
                playlist_id,
                track_id,
            } => {
                self.db.remove_from_playlist(playlist_id, track_id)?;
                self.refresh_playlists()?;
                if self.selected_playlist == Some(playlist_id) {
                    self.refresh_playlist_tracks()?;
                }
                self.status = "removed from playlist".into();
            }
            Action::PlayPlaylist { playlist_id } => {
                let rows = self.db.playlist_tracks(playlist_id)?;
                let Some(first) = rows.first() else {
                    anyhow::bail!("playlist is empty");
                };
                let context: Vec<i64> = rows.iter().map(|r| r.id).collect();
                self.player.play_track(first.id, &context, &self.db)?;
                self.status = self.now_playing_status();
            }
            Action::YtSearch { query } => {
                let query = query.trim().to_string();
                if query.is_empty() {
                    anyhow::bail!("search query is empty");
                }
                self.status = format!("searching YouTube for {query}…");
                self.yt_results.clear();
                let cfg = self.cfg.clone();
                let sem = self.sem.clone();
                let tx = self.download_sender();
                tokio::spawn(download::search(query, cfg, sem, tx));
            }
            Action::Download {
                url,
                title,
                uploader,
            } => {
                let id = self.next_job;
                self.next_job += 1;
                let row = DownloadRow {
                    id,
                    title: title.clone().unwrap_or_else(|| url.clone()),
                    state: DownloadState::Queued,
                    percent: 0.0,
                    detail: "queued".into(),
                    error: None,
                };
                self.downloads.insert(0, row);
                let cfg = self.cfg.clone();
                let sem = self.sem.clone();
                let tx = self.download_sender();
                let handle =
                    tokio::spawn(download::download(id, url, title, uploader, cfg, sem, tx));
                self.jobs.insert(id, handle);
                self.status = "download queued".into();
            }
            Action::CancelDownload { id } => {
                if let Some(handle) = self.jobs.remove(&id) {
                    handle.abort();
                }
                if let Some(row) = self.downloads.iter_mut().find(|r| r.id == id) {
                    row.state = DownloadState::Cancelled;
                    row.detail = "cancelled".into();
                }
                self.status = "download cancelled".into();
            }
            Action::ClearFinished => {
                self.downloads.retain(|r| !r.state.is_finished());
                self.status = "cleared finished downloads".into();
            }
            Action::Shutdown => {}
        }
        Ok(())
    }

    fn request_artwork(&mut self, track_id: i64, path: String) {
        self.art_path = None;
        self.art_track = None;
        let path = PathBuf::from(path);
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let artwork = art::extract(track_id, &path, mtime);
            let _ = tx.send(Internal::Artwork {
                track_id,
                path: artwork,
            });
        });
    }

    fn on_mpv(&mut self, event: MpvEvent) -> Result<()> {
        match event {
            MpvEvent::FileLoaded => {
                self.player.set_loaded(true);
                let artwork = self
                    .player
                    .current_id()
                    .and_then(|id| self.db.track(id).ok().flatten().map(|(_, path)| (id, path)));
                if let Some((track_id, path)) = artwork {
                    self.request_artwork(track_id, path);
                }
            }
            MpvEvent::EndFile { reason } => {
                // `stop`/`redirect` are emitted when mpv replaces a file;
                // they are not the end of playback.
                if let Some(was_playing) = self.player.file_ended(&reason) {
                    if reason == "eof" {
                        self.player.on_eof(&self.db)?;
                        if was_playing {
                            self.status = self.now_playing_status();
                        }
                    } else {
                        self.status = "playback error".into();
                    }
                    self.broadcast_state();
                }
            }
            MpvEvent::Position(seconds) => {
                self.player.position_ms = (seconds.max(0.0) * 1000.0) as u64;
            }
            MpvEvent::Duration(seconds) => {
                if seconds > 0.0 {
                    self.player.duration_ms = (seconds * 1000.0) as u64;
                }
            }
            MpvEvent::Pause(paused) => {
                self.player.set_paused(paused);
            }
            MpvEvent::Volume(volume) => {
                self.player.volume = volume.round().clamp(0.0, 100.0) as i64;
            }
            MpvEvent::Mute(muted) => {
                self.player.muted = muted;
            }
            MpvEvent::Idle | MpvEvent::Shutdown => {}
            MpvEvent::Error(message) => {
                self.status = message;
                self.broadcast_state();
            }
        }
        Ok(())
    }

    fn on_download(&mut self, event: download::Event) {
        match event {
            download::Event::SearchResults(results) => {
                let n = results.len();
                self.yt_results = results;
                self.status = format!("{n} results — Enter downloads, i searches again");
            }
            download::Event::SearchFailed(error) => {
                self.status = format!("search failed: {error}");
            }
            download::Event::JobUpdate(row) => {
                self.upsert_download(row);
            }
            download::Event::Complete { id, track } => {
                let title = track.title.clone();
                match self.db.insert(&track) {
                    Ok(_) => {
                        if let Err(e) = self.refresh_library() {
                            self.status =
                                format!("downloaded {title}, but library refresh failed: {e}");
                        } else {
                            self.status = format!("downloaded {title}");
                        }
                        self.upsert_download(DownloadRow {
                            id,
                            title,
                            state: DownloadState::Done,
                            percent: 100.0,
                            detail: "in library".into(),
                            error: None,
                        });
                        self.jobs.remove(&id);
                    }
                    Err(e) => {
                        self.status = format!("could not index download: {e}");
                        self.upsert_download(DownloadRow {
                            id,
                            title,
                            state: DownloadState::Failed,
                            percent: 100.0,
                            detail: "indexing failed".into(),
                            error: Some(e.to_string()),
                        });
                    }
                }
            }
            download::Event::Failed { id, error } => {
                if let Some(row) = self.downloads.iter_mut().find(|r| r.id == id) {
                    row.state = DownloadState::Failed;
                    row.error = Some(error.clone());
                    row.detail = "failed".into();
                }
                self.status = format!("download failed: {error}");
                self.jobs.remove(&id);
            }
        }
    }

    fn upsert_download(&mut self, row: DownloadRow) {
        if let Some(existing) = self.downloads.iter_mut().find(|r| r.id == row.id) {
            *existing = row;
        } else {
            self.downloads.insert(0, row);
        }
    }

    /// Bridge a downloader's event channel into the daemon's actor channel.
    fn download_sender(&self) -> mpsc::UnboundedSender<download::Event> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let daemon_tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if daemon_tx.send(Internal::Download(event)).is_err() {
                    break;
                }
            }
        });
        tx
    }

    fn refresh_library(&mut self) -> Result<()> {
        self.tracks = self.db.rows()?;
        Ok(())
    }

    fn refresh_playlists(&mut self) -> Result<()> {
        self.playlists = self.db.playlists()?;
        Ok(())
    }

    fn refresh_playlist_tracks(&mut self) -> Result<()> {
        self.playlist_tracks = match self.selected_playlist {
            Some(id) => self.db.playlist_tracks(id)?,
            None => Vec::new(),
        };
        Ok(())
    }

    fn now_playing_status(&self) -> String {
        match self
            .player
            .current_id()
            .and_then(|id| self.db.track(id).ok().flatten())
        {
            Some((row, _)) => format!("playing {}", row.title),
            None => "ready".into(),
        }
    }

    /// Resume the selected queue entry, or start the first library track.
    fn start_current(&mut self) -> Result<()> {
        if let Some(index) = self.player.pos
            && index < self.player.queue.len()
        {
            return self.player.play_queue_index(index, &self.db);
        }
        if let Some(first) = self.tracks.first() {
            return self.player.play_track(first.id, &[first.id], &self.db);
        }
        anyhow::bail!("nothing to play — the library is empty")
    }

    fn transport(&self) -> Transport {
        let track = self
            .player
            .current_id()
            .and_then(|id| self.db.track(id).ok().flatten())
            .map(|(row, _)| row);
        let art_path = match track.as_ref() {
            Some(track) if self.art_track == Some(track.id) => self.art_path.clone(),
            _ => None,
        };
        Transport {
            track,
            art_path,
            position_ms: self.player.position_ms,
            duration_ms: self.player.duration_ms,
            playing: self.player.playing,
            volume: self.player.volume,
            muted: self.player.muted,
            shuffle: self.player.shuffle,
            repeat: self.player.repeat,
        }
    }

    fn snapshot(&self) -> ViewModel {
        let queue = self
            .db
            .tracks_by_ids(&self.player.queue)
            .unwrap_or_default();
        ViewModel {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            protocol: melos_proto::PROTOCOL_VERSION,
            tracks: self.tracks.clone(),
            queue,
            queue_pos: self.player.pos,
            playlists: self.playlists.clone(),
            playlist_tracks: self.playlist_tracks.clone(),
            selected_playlist: self.selected_playlist,
            downloads: self.downloads.clone(),
            yt_results: self.yt_results.clone(),
            transport: self.transport(),
            stats: self.db.stats().unwrap_or_default(),
            status: self.status.clone(),
        }
    }

    fn broadcast_state(&mut self) {
        self.broadcast_state_seek(None);
    }

    fn broadcast_state_seek(&mut self, seeked: Option<u64>) {
        self.sync_mpris();
        let snapshot = self.snapshot();
        self.send_all(ServerMsg::State { snapshot });
        if let Some(server) = &self.mpris {
            let changed = self
                .mpris_info
                .as_ref()
                .and_then(|info| {
                    info.lock()
                        .ok()
                        .map(|info| mpris::changed_properties(&info))
                })
                .unwrap_or_default();
            let server = server.clone();
            tokio::spawn(mpris::announce(server, changed, seeked));
        }
    }

    fn sync_mpris(&self) {
        let Some(shared) = &self.mpris_info else {
            return;
        };
        let track = self
            .player
            .current_id()
            .and_then(|id| self.db.track(id).ok().flatten())
            .map(|(row, _)| TrackInfo {
                id: row.id,
                title: row.title,
                artist: row.artist,
                album: row.album.unwrap_or_default(),
                duration_ms: row.duration_ms,
            });
        let mut info = match shared.lock() {
            Ok(info) => info,
            Err(poisoned) => poisoned.into_inner(),
        };
        *info = Info {
            track,
            playing: self.player.playing,
            loaded: self.player.is_loaded(),
            position_ms: self.player.position_ms,
            duration_ms: self.player.duration_ms,
            volume: self.player.volume,
            shuffle: self.player.shuffle,
            repeat: self.player.repeat,
        };
    }

    fn send_all(&mut self, msg: ServerMsg) {
        self.clients.retain(|tx| tx.send(msg.clone()).is_ok());
    }

    fn persist(&mut self) {
        let state = Persisted {
            queue: self.player.queue.clone(),
            pos: self.player.pos,
            position_ms: self.player.position_ms,
            volume: Some(self.player.volume),
            muted: self.player.muted,
            shuffle: self.player.shuffle,
            repeat: self.player.repeat,
            selected_playlist: self.selected_playlist,
        };
        if let Err(e) = save_state(&state) {
            tracing::warn!("could not persist state: {e}");
        }
    }

    pub fn announce_shutdown(&mut self) {
        self.send_all(ServerMsg::Shutdown);
    }

    pub async fn shutdown(&mut self) {
        self.listen_tick();
        self.flush_listening();
        self.player.stop();
        if let Some(mpv) = self.player.mpv_mut() {
            mpv.shutdown().await;
        }
        self.persist();
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Persisted {
    queue: Vec<i64>,
    pos: Option<usize>,
    position_ms: u64,
    volume: Option<i64>,
    muted: bool,
    shuffle: bool,
    repeat: Repeat,
    selected_playlist: Option<i64>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            queue: Vec::new(),
            pos: None,
            position_ms: 0,
            volume: Some(70),
            muted: false,
            shuffle: false,
            repeat: Repeat::Off,
            selected_playlist: None,
        }
    }
}

fn file_within(base: &Path, target: &Path) -> bool {
    target.starts_with(base)
}

fn load_state() -> Option<Persisted> {
    let path = paths::state_file();
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

fn save_state(state: &Persisted) -> Result<()> {
    let path = paths::state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp: PathBuf = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[derive(Debug, Clone)]
struct ListenEntry {
    track_id: i64,
    title: String,
    artist: String,
    ms: u64,
}

/// Accumulates wall-clock listening time between ticks, so detached playback
/// is counted as well.
struct ListenTracker {
    last: Instant,
    entry: Option<ListenEntry>,
}

impl ListenTracker {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(now: Instant) -> Self {
        Self {
            last: now,
            entry: None,
        }
    }

    /// Advance the clock. Returns a completed entry when the active track
    /// changes or playback stops.
    fn advance(
        &mut self,
        now: Instant,
        active: Option<(i64, String, String)>,
    ) -> Option<ListenEntry> {
        let elapsed = now.saturating_duration_since(self.last).as_millis() as u64;
        self.last = now;
        let same = match (&self.entry, &active) {
            (Some(entry), Some((id, _, _))) => entry.track_id == *id,
            (None, None) => true,
            _ => false,
        };
        if same {
            if let (Some(entry), Some((_, title, artist))) = (&mut self.entry, active) {
                entry.ms = entry.ms.saturating_add(elapsed);
                entry.title = title;
                entry.artist = artist;
            }
            return None;
        }
        let old = self.entry.take();
        self.entry = active.map(|(track_id, title, artist)| ListenEntry {
            track_id,
            title,
            artist,
            ms: 0,
        });
        old.filter(|entry| entry.ms > 0)
    }

    fn take(&mut self) -> Option<ListenEntry> {
        self.entry.take().filter(|entry| entry.ms > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: i64) -> Option<(i64, String, String)> {
        Some((id, format!("track {id}"), "Artist".into()))
    }

    #[test]
    fn listening_time_accumulates_only_while_playing() {
        let start = Instant::now();
        let mut tracker = ListenTracker::new_at(start);
        assert!(
            tracker
                .advance(start + Duration::from_secs(1), info(1))
                .is_none()
        );
        assert!(
            tracker
                .advance(start + Duration::from_secs(3), info(1))
                .is_none()
        );
        let flushed = tracker
            .advance(start + Duration::from_secs(3), None)
            .expect("pausing flushes the current track");
        assert_eq!(flushed.track_id, 1);
        assert_eq!(flushed.ms, 2_000);
        assert!(
            tracker
                .advance(start + Duration::from_secs(9), None)
                .is_none()
        );
    }

    #[test]
    fn switching_tracks_flushes_the_previous_track() {
        let start = Instant::now();
        let mut tracker = ListenTracker::new_at(start);
        tracker.advance(start, info(1));
        tracker.advance(start + Duration::from_secs(2), info(1));
        let flushed = tracker
            .advance(start + Duration::from_secs(2), info(2))
            .expect("switching tracks flushes the old one");
        assert_eq!(flushed.track_id, 1);
        assert_eq!(flushed.ms, 2_000);
        tracker.advance(start + Duration::from_secs(4), info(2));
        let flushed = tracker.take().expect("shutdown flushes the last track");
        assert_eq!(flushed.track_id, 2);
        assert_eq!(flushed.ms, 2_000);
    }

    #[test]
    fn delete_guard_only_accepts_files_under_the_music_dir() {
        let base = std::env::temp_dir().join(format!("melos-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("music")).unwrap();
        std::fs::write(base.join("music/a.opus"), b"x").unwrap();
        std::fs::write(base.join("outside.opus"), b"x").unwrap();

        let music = std::fs::canonicalize(base.join("music")).unwrap();
        let inside = std::fs::canonicalize(base.join("music/a.opus")).unwrap();
        let outside = std::fs::canonicalize(base.join("outside.opus")).unwrap();
        assert!(file_within(&music, &inside));
        assert!(!file_within(&music, &outside));
        let _ = std::fs::remove_dir_all(&base);
    }
}
