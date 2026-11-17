//! MPRIS integration: exposes playback to desktop environments and media keys.

use std::sync::{Arc, Mutex};

use melos_proto::{Action, Repeat};
use mpris_server::{
    LoopStatus, Metadata, PlaybackStatus, PlayerInterface, Property, RootInterface, Server, Signal,
    Time, TrackId, Volume, zbus::fdo,
};
use tokio::sync::mpsc;

use crate::state::Internal;

#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub id: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Info {
    pub track: Option<TrackInfo>,
    pub playing: bool,
    pub loaded: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub volume: i64,
    pub shuffle: bool,
    pub repeat: Repeat,
}

pub struct Mpris {
    info: Arc<Mutex<Info>>,
    tx: mpsc::UnboundedSender<Internal>,
}

impl Mpris {
    fn send(&self, action: Action) -> fdo::Result<()> {
        self.tx
            .send(Internal::Action(action))
            .map_err(|_| fdo::Error::Failed("melosd is shutting down".into()))
    }

    fn snapshot(&self) -> Info {
        self.info.lock().map(|i| i.clone()).unwrap_or_default()
    }
}

pub async fn start(
    tx: mpsc::UnboundedSender<Internal>,
) -> anyhow::Result<(Arc<Server<Mpris>>, Arc<Mutex<Info>>)> {
    let info = Arc::new(Mutex::new(Info::default()));
    let imp = Mpris {
        info: info.clone(),
        tx,
    };
    let server = Server::new("melos", imp).await?;
    Ok((Arc::new(server), info))
}

fn loop_status(repeat: Repeat) -> LoopStatus {
    match repeat {
        Repeat::Off => LoopStatus::None,
        Repeat::All => LoopStatus::Playlist,
        Repeat::One => LoopStatus::Track,
    }
}

fn repeat_of(status: LoopStatus) -> Repeat {
    match status {
        LoopStatus::None => Repeat::Off,
        LoopStatus::Track => Repeat::One,
        LoopStatus::Playlist => Repeat::All,
    }
}

impl RootInterface for Mpris {
    async fn raise(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        self.tx
            .send(Internal::Action(Action::Shutdown))
            .map_err(|_| fdo::Error::Failed("melosd is shutting down".into()))
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> mpris_server::zbus::Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok("melos".into())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok("melos".into())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }
}

impl PlayerInterface for Mpris {
    async fn next(&self) -> fdo::Result<()> {
        self.send(Action::Next)
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.send(Action::Prev)
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.send(Action::SetPaused { paused: true })
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        self.send(Action::PauseToggle)
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.send(Action::Stop)
    }

    async fn play(&self) -> fdo::Result<()> {
        self.send(Action::SetPaused { paused: false })
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        self.send(Action::SeekBy {
            seconds: offset.as_micros() / 1_000_000,
        })
    }

    async fn set_position(&self, track_id: TrackId, position: Time) -> fdo::Result<()> {
        let info = self.snapshot();
        let Some(track) = info.track else {
            return Err(fdo::Error::Failed("nothing is playing".into()));
        };
        let expected = format!("/org/melos/track/{}", track.id);
        if track_id.to_string() != expected {
            return Err(fdo::Error::Failed("unknown track id".into()));
        }
        self.send(Action::SeekTo {
            seconds: position.as_micros().max(0) as u64 / 1_000_000,
        })
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("open_uri is not supported".into()))
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        let info = self.snapshot();
        Ok(if info.track.is_none() || !info.loaded {
            PlaybackStatus::Stopped
        } else if info.playing {
            PlaybackStatus::Playing
        } else {
            PlaybackStatus::Paused
        })
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(loop_status(self.snapshot().repeat))
    }

    async fn set_loop_status(&self, status: LoopStatus) -> mpris_server::zbus::Result<()> {
        self.send(Action::SetRepeat {
            mode: repeat_of(status),
        })
        .map_err(|e| mpris_server::zbus::Error::Failure(e.to_string()))
    }

    async fn rate(&self) -> fdo::Result<f64> {
        Ok(1.0)
    }

    async fn set_rate(&self, _rate: f64) -> mpris_server::zbus::Result<()> {
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().shuffle)
    }

    async fn set_shuffle(&self, shuffle: bool) -> mpris_server::zbus::Result<()> {
        self.send(Action::SetShuffle { on: shuffle })
            .map_err(|e| mpris_server::zbus::Error::Failure(e.to_string()))
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(metadata(&self.snapshot()))
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.snapshot().volume as f64 / 100.0)
    }

    async fn set_volume(&self, volume: Volume) -> mpris_server::zbus::Result<()> {
        self.send(Action::SetVolume {
            volume: (volume * 100.0).round() as i64,
        })
        .map_err(|e| mpris_server::zbus::Error::Failure(e.to_string()))
    }

    async fn position(&self) -> fdo::Result<Time> {
        Ok(Time::from_millis(self.snapshot().position_ms as i64))
    }

    async fn minimum_rate(&self) -> fdo::Result<f64> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<f64> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().track.is_some())
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().track.is_some())
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().track.is_some())
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().track.is_some())
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

fn metadata(info: &Info) -> Metadata {
    let Some(track) = &info.track else {
        return Metadata::builder().trackid(TrackId::NO_TRACK).build();
    };
    let track_id = match TrackId::try_from(format!("/org/melos/track/{}", track.id)) {
        Ok(id) => id,
        Err(_) => return Metadata::builder().trackid(TrackId::NO_TRACK).build(),
    };
    let mut builder = Metadata::builder()
        .trackid(track_id)
        .title(track.title.clone())
        .artist([track.artist.clone()]);
    if !track.album.is_empty() {
        builder = builder.album(track.album.clone());
    }
    let length = info.duration_ms.max(track.duration_ms);
    if length > 0 {
        builder = builder.length(Time::from_millis(length as i64));
    }
    builder.build()
}

/// A full property snapshot for `PropertiesChanged`.
pub fn changed_properties(info: &Info) -> Vec<Property> {
    vec![
        Property::PlaybackStatus(if info.track.is_none() || !info.loaded {
            PlaybackStatus::Stopped
        } else if info.playing {
            PlaybackStatus::Playing
        } else {
            PlaybackStatus::Paused
        }),
        Property::LoopStatus(loop_status(info.repeat)),
        Property::Shuffle(info.shuffle),
        Property::Metadata(metadata(info)),
        Property::Volume(info.volume as f64 / 100.0),
        Property::CanGoNext(info.track.is_some()),
        Property::CanGoPrevious(info.track.is_some()),
        Property::CanPlay(true),
        Property::CanPause(info.track.is_some()),
        Property::CanSeek(info.track.is_some()),
    ]
}

pub async fn announce(server: Arc<Server<Mpris>>, changed: Vec<Property>, seeked: Option<u64>) {
    let _ = server.properties_changed(changed).await;
    if let Some(position_ms) = seeked {
        let _ = server
            .emit(Signal::Seeked {
                position: Time::from_millis(position_ms as i64),
            })
            .await;
    }
}
