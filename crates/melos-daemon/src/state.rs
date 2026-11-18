//! Authoritative daemon state and the action reducer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use melos_proto::{
    Action, DownloadRow, DownloadState, LyricsView, PlaylistRow, Repeat, ServerMsg, TrackRow,
    Transport, ViewModel, YtResultRow, config::Config, lyrics::has_timestamps,
    lyrics::plain_from_lrc, paths,
};
use mpris_server::Server;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::art;
use crate::db::Db;
use crate::download;
use crate::lyrics;
use crate::mpris::{self, Info, Mpris, TrackInfo};
use crate::mpv::MpvEvent;
use crate::player::Player;

pub enum Internal {
    Action(Action),
    Mpv(MpvEvent),
    Download(download::Event),
    Client(mpsc::UnboundedSender<ServerMsg>),
    Mpris(Arc<Server<Mpris>>, Arc<Mutex<Info>>),
    Artwork {
        track_id: i64,
        path: Option<String>,
    },
    Lyrics {
        track_id: i64,
        result: Result<lyrics::FetchedLyrics, String>,
    },
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
    download_urls: HashMap<u64, String>,
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
    last_listen_flush: Instant,
    art_path: Option<String>,
    art_track: Option<i64>,
    lyrics: Option<LyricsView>,
    lyrics_track: Option<i64>,
    lyrics_loading: bool,
    lyrics_error: Option<String>,
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
            download_urls: HashMap::new(),
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
            last_listen_flush: Instant::now(),
            art_path: None,
            art_track: None,
            lyrics: None,
            lyrics_track: None,
            lyrics_loading: false,
            lyrics_error: None,
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
        if let Some(entry) = self.listening.split()
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
            Internal::Lyrics { track_id, result } => {
                if self.player.current_id() == Some(track_id) {
                    self.lyrics_loading = false;
                    match result {
                        Ok(fetched) => {
                            self.lyrics = self
                                .db
                                .save_lyrics(
                                    track_id,
                                    &fetched.plain,
                                    fetched.synced.as_deref(),
                                    &fetched.source,
                                )
                                .ok();
                            self.lyrics_error = None;
                        }
                        Err(error) => {
                            // Keep any lyrics we already have (for example a
                            // manual paste) when a refetch misses.
                            self.status = error.clone();
                            self.lyrics_error = Some(error);
                        }
                    }
                    self.lyrics_track = Some(track_id);
                    self.push_lyrics();
                    self.broadcast_state();
                }
            }
            Internal::Note(note) => {
                self.status = note;
                self.broadcast_state();
            }
            Internal::Tick => {
                self.listen_tick();
                let now = Instant::now();
                if self.player.playing
                    && self.player.is_loaded()
                    && listen_flush_due(self.last_listen_flush, now)
                {
                    self.flush_listening();
                    self.last_listen_flush = now;
                    self.transport_dirty = true;
                }
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
            Action::MoveQueue { start, len, delta } => {
                if !self.player.move_queue(start, len, delta) {
                    anyhow::bail!("the selected queue block cannot move further");
                }
                self.status = "queue reordered".into();
            }
            Action::MoveLibrary { start, len, delta } => {
                if !self.db.move_library(start, len, delta)? {
                    anyhow::bail!("the selected library block cannot move further");
                }
                self.refresh_library()?;
                self.status = "library reordered".into();
            }
            Action::MovePlaylistTracks {
                playlist_id,
                start,
                len,
                delta,
            } => {
                if !self
                    .db
                    .move_playlist_tracks(playlist_id, start, len, delta)?
                {
                    anyhow::bail!("the selected playlist block cannot move further");
                }
                if self.selected_playlist == Some(playlist_id) {
                    self.refresh_playlist_tracks()?;
                }
                self.status = "playlist reordered".into();
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
                if let Some(existing) = self.active_download(&url) {
                    let name = self
                        .downloads
                        .iter()
                        .find(|row| row.id == existing)
                        .map(|row| row.title.clone())
                        .unwrap_or_else(|| url.clone());
                    self.status = format!("already downloading {name}");
                    return Ok(());
                }
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
                let dedupe_url = url.clone();
                let handle =
                    tokio::spawn(download::download(id, url, title, uploader, cfg, sem, tx));
                self.jobs.insert(id, handle);
                self.download_urls.insert(id, dedupe_url);
                self.status = "download queued".into();
            }
            Action::CancelDownload { id } => {
                if let Some(handle) = self.jobs.remove(&id) {
                    handle.abort();
                }
                self.download_urls.remove(&id);
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
            Action::RequestLyrics { force } => {
                let track_id = self
                    .player
                    .current_id()
                    .ok_or_else(|| anyhow::anyhow!("nothing is playing"))?;
                if !force && let Ok(Some(cached)) = self.db.get_lyrics(track_id) {
                    self.lyrics = Some(cached);
                    self.lyrics_track = Some(track_id);
                    self.lyrics_loading = false;
                    self.lyrics_error = None;
                    self.push_lyrics();
                    return Ok(());
                }
                let (track, _) = self
                    .db
                    .track(track_id)?
                    .ok_or_else(|| anyhow::anyhow!("track not found"))?;
                self.lyrics_track = Some(track_id);
                self.lyrics_loading = true;
                self.lyrics_error = None;
                self.push_lyrics();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || lyrics::fetch(&track))
                        .await
                        .unwrap_or_else(|error| Err(format!("lyrics lookup failed: {error}")));
                    let _ = tx.send(Internal::Lyrics { track_id, result });
                });
            }
            Action::SaveLyrics { track_id, text } => {
                let text = text.trim();
                if text.is_empty() {
                    anyhow::bail!("lyrics are empty");
                }
                let synced = has_timestamps(text);
                let plain = if synced {
                    plain_from_lrc(text)
                } else {
                    text.to_string()
                };
                let saved =
                    self.db
                        .save_lyrics(track_id, &plain, synced.then_some(text), "manual")?;
                if self.player.current_id() == Some(track_id) {
                    self.lyrics = Some(saved);
                    self.lyrics_track = Some(track_id);
                    self.lyrics_loading = false;
                    self.lyrics_error = None;
                    self.push_lyrics();
                }
                self.status = "lyrics saved".into();
            }
            Action::SetLyricsOffset {
                track_id,
                offset_ms,
            } => {
                let offset = offset_ms.clamp(-60_000, 60_000);
                self.db.set_lyrics_offset(track_id, offset)?;
                if self.player.current_id() == Some(track_id) {
                    if let Some(lyrics) = self.lyrics.as_mut() {
                        lyrics.offset_ms = offset;
                    }
                    self.push_lyrics();
                }
                self.status = format!("lyrics offset {:+} ms", offset);
            }
            Action::Shutdown => {}
        }
        Ok(())
    }

    fn load_cached_lyrics(&mut self, track_id: Option<i64>) {
        self.lyrics = None;
        self.lyrics_track = track_id;
        self.lyrics_loading = false;
        self.lyrics_error = None;
        if let Some(id) = track_id {
            self.lyrics = self.db.get_lyrics(id).ok().flatten();
        }
    }

    fn push_lyrics(&mut self) {
        let track_id = self
            .lyrics_track
            .or_else(|| self.player.current_id())
            .unwrap_or(0);
        self.send_all(ServerMsg::Lyrics {
            track_id,
            lyrics: self.lyrics.clone(),
            loading: self.lyrics_loading,
            error: self.lyrics_error.clone(),
        });
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
                let track_id = self.player.current_id();
                self.load_cached_lyrics(track_id);
                if let Some(track_id) = track_id
                    && let Some((_, path)) = self.db.track(track_id).ok().flatten()
                {
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
                        self.download_urls.remove(&id);
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
                        self.jobs.remove(&id);
                        self.download_urls.remove(&id);
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
                self.download_urls.remove(&id);
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

    fn active_download(&self, url: &str) -> Option<u64> {
        self.download_urls
            .iter()
            .find_map(|(id, queued)| (queued == url).then_some(*id))
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
            lyrics: self.lyrics.clone(),
            lyrics_loading: self.lyrics_loading,
            lyrics_error: self.lyrics_error.clone(),
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

    /// Return the time accumulated so far without forgetting which track is
    /// playing, so the next tick continues the same entry.
    fn split(&mut self) -> Option<ListenEntry> {
        let entry = self.entry.as_mut()?;
        if entry.ms == 0 {
            return None;
        }
        let flushed = entry.clone();
        entry.ms = 0;
        Some(flushed)
    }
}

const LISTEN_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

fn listen_flush_due(last: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last) >= LISTEN_FLUSH_INTERVAL
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
        let flushed = tracker.split().expect("shutdown flushes the last track");
        assert_eq!(flushed.track_id, 2);
        assert_eq!(flushed.ms, 2_000);
    }

    #[test]
    fn periodic_flush_keeps_the_active_track_and_never_loses_elapsed_time() {
        let start = Instant::now();
        let mut tracker = ListenTracker::new_at(start);
        tracker.advance(start, info(1));
        tracker.advance(start + Duration::from_secs(5), info(1));
        let flushed = tracker.split().expect("30s tick flushes");
        assert_eq!(flushed.track_id, 1);
        assert_eq!(flushed.ms, 5_000);
        assert!(tracker.split().is_none(), "nothing new since the flush");

        tracker.advance(start + Duration::from_secs(7), info(1));
        let flushed = tracker.split().expect("second tick flushes the remainder");
        assert_eq!(flushed.track_id, 1);
        assert_eq!(flushed.ms, 2_000);
    }

    #[test]
    fn listening_flush_is_due_after_thirty_seconds() {
        let start = Instant::now();
        assert!(!listen_flush_due(start, start + Duration::from_secs(29)));
        assert!(listen_flush_due(start, start + Duration::from_secs(30)));
    }

    fn test_daemon(name: &str) -> (Daemon, PathBuf) {
        let base = std::env::temp_dir().join(format!("melos-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("music")).unwrap();
        let cfg = Config {
            music_dir: base.join("music").display().to_string(),
            ..Config::default()
        };
        let db = Db::open(&base.join("library.db")).unwrap();
        let player = Player::new(None, 70, false, Repeat::Off);
        let (tx, _rx) = mpsc::unbounded_channel();
        let daemon = Daemon {
            cfg: Arc::new(cfg),
            db,
            player,
            tracks: Vec::new(),
            playlists: Vec::new(),
            playlist_tracks: Vec::new(),
            selected_playlist: None,
            downloads: Vec::new(),
            jobs: HashMap::new(),
            download_urls: HashMap::new(),
            next_job: 1,
            yt_results: Vec::new(),
            clients: Vec::new(),
            tx,
            sem: Arc::new(Semaphore::new(1)),
            status: "test".into(),
            transport_dirty: false,
            last_persist: Instant::now(),
            mpris: None,
            mpris_info: None,
            listening: ListenTracker::new(),
            last_listen_flush: Instant::now(),
            art_path: None,
            art_track: None,
            lyrics: None,
            lyrics_track: None,
            lyrics_loading: false,
            lyrics_error: None,
        };
        (daemon, base)
    }

    fn insert_file(daemon: &Daemon, name: &str) -> (i64, PathBuf) {
        let path = daemon.cfg.music_dir_path().join(format!("{name}.opus"));
        std::fs::write(&path, b"audio").unwrap();
        let id = daemon
            .db
            .insert(&crate::db::NewTrack {
                path: path.display().to_string(),
                yt_id: name.into(),
                title: name.into(),
                artist: "Artist".into(),
                album: None,
                duration_ms: 60_000,
                mtime: 1,
            })
            .unwrap();
        (id, path)
    }

    #[test]
    fn reorder_actions_update_the_visible_library_playlist_and_queue() {
        let (mut daemon, base) = test_daemon("reorder");
        let (b, _) = insert_file(&daemon, "b");
        let (a, _) = insert_file(&daemon, "a");
        daemon.refresh_library().unwrap();
        assert_eq!(
            daemon
                .tracks
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );

        daemon
            .apply(Action::MoveLibrary {
                start: 1,
                len: 1,
                delta: -1,
            })
            .unwrap();
        assert_eq!(
            daemon
                .tracks
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );

        let playlist = daemon.db.create_playlist("Mix").unwrap();
        daemon.db.add_to_playlist(playlist, b).unwrap();
        daemon.db.add_to_playlist(playlist, a).unwrap();
        daemon.selected_playlist = Some(playlist);
        daemon.refresh_playlist_tracks().unwrap();
        daemon
            .apply(Action::MovePlaylistTracks {
                playlist_id: playlist,
                start: 0,
                len: 1,
                delta: 1,
            })
            .unwrap();
        assert_eq!(
            daemon
                .playlist_tracks
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );

        daemon.player.queue = vec![a, b];
        daemon
            .apply(Action::MoveQueue {
                start: 0,
                len: 1,
                delta: 1,
            })
            .unwrap();
        assert_eq!(daemon.player.queue, vec![b, a]);

        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn failed_refetch_keeps_existing_lyrics() {
        let (mut daemon, base) = test_daemon("lyrics-preserve");
        let (id, _) = insert_file(&daemon, "song");
        daemon.player.play_track(id, &[id], &daemon.db).unwrap();
        let saved = daemon
            .db
            .save_lyrics(id, "line one", None, "manual")
            .unwrap();
        daemon.lyrics = Some(saved);
        daemon.lyrics_track = Some(id);

        daemon
            .handle(Internal::Lyrics {
                track_id: id,
                result: Err("no lyrics found on LRCLIB".into()),
            })
            .await;

        assert!(daemon.lyrics.is_some());
        assert_eq!(
            daemon.lyrics_error.as_deref(),
            Some("no lyrics found on LRCLIB")
        );
        assert_eq!(daemon.status, "no lyrics found on LRCLIB");
        let _ = std::fs::remove_dir_all(base);
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

    #[test]
    fn active_downloads_are_matched_by_url() {
        let (mut daemon, _base) = test_daemon("active-download");
        daemon
            .download_urls
            .insert(7, "https://www.youtube.com/watch?v=r1PPckDMMSg".to_string());
        assert_eq!(
            daemon.active_download("https://www.youtube.com/watch?v=r1PPckDMMSg"),
            Some(7)
        );
        assert_eq!(
            daemon.active_download("https://www.youtube.com/watch?v=other"),
            None
        );
        daemon.download_urls.remove(&7);
        assert_eq!(
            daemon.active_download("https://www.youtube.com/watch?v=r1PPckDMMSg"),
            None
        );
    }

    #[test]
    fn delete_action_removes_the_file_membership_and_queue_entry() {
        let (mut daemon, base) = test_daemon("delete");
        let (id, path) = insert_file(&daemon, "a");
        let playlist = daemon.db.create_playlist("Favorites").unwrap();
        daemon.db.add_to_playlist(playlist, id).unwrap();
        daemon.player.play_track(id, &[id], &daemon.db).unwrap();

        daemon
            .apply(Action::DeleteTrack {
                track_id: id,
                delete_file: true,
            })
            .unwrap();

        assert!(!path.exists());
        assert!(daemon.db.track(id).unwrap().is_none());
        assert!(daemon.db.playlist_tracks(playlist).unwrap().is_empty());
        assert!(daemon.player.queue.is_empty());
        assert_eq!(daemon.player.current_id(), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn delete_action_refuses_outside_and_missing_files_without_forgetting_the_track() {
        let (mut daemon, base) = test_daemon("delete-guard");
        let outside = base.join("outside.opus");
        std::fs::write(&outside, b"audio").unwrap();
        let id = daemon
            .db
            .insert(&crate::db::NewTrack {
                path: outside.display().to_string(),
                yt_id: "outside".into(),
                title: "outside".into(),
                artist: "Artist".into(),
                album: None,
                duration_ms: 1,
                mtime: 1,
            })
            .unwrap();
        assert!(
            daemon
                .apply(Action::DeleteTrack {
                    track_id: id,
                    delete_file: true,
                })
                .is_err()
        );
        assert!(outside.exists());
        assert!(daemon.db.track(id).unwrap().is_some());

        let missing = daemon.cfg.music_dir_path().join("missing.opus");
        let id = daemon
            .db
            .insert(&crate::db::NewTrack {
                path: missing.display().to_string(),
                yt_id: "missing".into(),
                title: "missing".into(),
                artist: "Artist".into(),
                album: None,
                duration_ms: 1,
                mtime: 1,
            })
            .unwrap();
        assert!(
            daemon
                .apply(Action::DeleteTrack {
                    track_id: id,
                    delete_file: true,
                })
                .is_err()
        );
        assert!(daemon.db.track(id).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn file_loaded_does_not_undo_a_user_pause() {
        let (mut daemon, base) = test_daemon("paused");
        let (id, _) = insert_file(&daemon, "a");
        daemon.player.play_track(id, &[id], &daemon.db).unwrap();
        daemon.player.set_playing(false);
        assert!(!daemon.player.playing);
        daemon.on_mpv(MpvEvent::FileLoaded).unwrap();
        assert!(daemon.player.is_loaded());
        assert!(!daemon.player.playing);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn tick_flushes_listening_every_thirty_seconds() {
        let (mut daemon, base) = test_daemon("flush");
        let (id, _) = insert_file(&daemon, "a");
        daemon.player.play_track(id, &[id], &daemon.db).unwrap();
        daemon.listening = ListenTracker {
            last: Instant::now(),
            entry: Some(ListenEntry {
                track_id: id,
                title: "a".into(),
                artist: "Artist".into(),
                ms: 30_000,
            }),
        };
        daemon.last_listen_flush = Instant::now() - Duration::from_secs(31);

        daemon.handle(Internal::Tick).await;

        let stats = daemon.db.stats().unwrap();
        assert!(stats.total_ms >= 30_000);
        assert_eq!(daemon.listening.entry.as_ref().unwrap().ms, 0);
        let _ = std::fs::remove_dir_all(&base);
    }
}
