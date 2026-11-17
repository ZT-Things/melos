//! Control of a single mpv process over its JSON IPC socket.
//!
//! mpv is spawned once and kept warm; the daemon owns the queue and feeds it
//! one file at a time. All commands are newline-delimited JSON on a unix socket.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum MpvEvent {
    FileLoaded,
    EndFile { reason: String },
    Position(f64),
    Duration(f64),
    Pause(bool),
    Volume(f64),
    Mute(bool),
    Idle,
    Shutdown,
    Error(String),
}

pub struct MpvHandle {
    tx: mpsc::UnboundedSender<String>,
    socket: PathBuf,
    child: Child,
}

impl MpvHandle {
    pub fn spawn(
        socket: &Path,
        extra_args: &[String],
        events: mpsc::UnboundedSender<MpvEvent>,
    ) -> Result<Self> {
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let _ = std::fs::remove_file(socket);

        let mut cmd = Command::new("mpv");
        cmd.arg("--idle=yes")
            .arg("--no-video")
            .arg("--no-terminal")
            .arg("--force-window=no")
            .arg("--audio-display=no")
            .arg("--prefetch-playlist=yes")
            .arg("--gapless-audio=yes")
            .arg(format!("--input-ipc-server={}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for arg in extra_args {
            cmd.arg(arg);
        }
        let child = cmd.spawn().context(
            "could not start mpv (is it installed?). Playback is unavailable without it",
        )?;

        // Do not block daemon startup on mpv's IPC socket: the UI must be
        // interactive immediately. Commands are queued until the socket is
        // ready, then the writer task drains them in order.
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        for (id, prop) in [
            (1, "time-pos"),
            (2, "duration"),
            (3, "pause"),
            (4, "volume"),
            (5, "mute"),
            (6, "idle-active"),
        ] {
            let _ = tx.send(json!({ "command": ["observe_property", id, prop] }).to_string());
        }
        tokio::spawn(connect_task(socket.to_path_buf(), rx, events.clone()));

        Ok(Self {
            tx,
            socket: socket.to_path_buf(),
            child,
        })
    }

    pub fn send(&self, value: Value) {
        let _ = self.tx.send(value.to_string());
    }

    pub fn load(&self, path: &str) {
        self.send(json!({ "command": ["loadfile", path, "replace"] }));
        self.set_pause(false);
    }

    pub fn set_pause(&self, pause: bool) {
        self.send(json!({ "command": ["set_property", "pause", pause] }));
    }

    pub fn seek_relative(&self, seconds: f64) {
        self.send(json!({ "command": ["seek", seconds, "relative"] }));
    }

    pub fn seek_absolute(&self, seconds: f64) {
        self.send(json!({ "command": ["seek", seconds, "absolute"] }));
    }

    pub fn set_volume(&self, volume: i64) {
        self.send(json!({ "command": ["set_property", "volume", volume] }));
    }

    pub fn set_mute(&self, mute: bool) {
        self.send(json!({ "command": ["set_property", "mute", mute] }));
    }

    pub fn stop(&self) {
        self.send(json!({ "command": ["stop"] }));
    }

    pub async fn shutdown(&mut self) {
        self.send(json!({ "command": ["quit"] }));
        let _ = tokio::time::timeout(Duration::from_millis(300), self.child.wait()).await;
        let _ = self.child.kill().await;
        let _ = std::fs::remove_file(&self.socket);
    }
}

async fn connect_task(
    socket: PathBuf,
    rx: mpsc::UnboundedReceiver<String>,
    events: mpsc::UnboundedSender<MpvEvent>,
) {
    // mpv creates the socket shortly after starting.
    let mut stream = None;
    for _ in 0..150 {
        if let Ok(s) = UnixStream::connect(&socket).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let Some(stream) = stream else {
        let _ = events.send(MpvEvent::Error(
            "mpv IPC socket did not appear; playback unavailable".into(),
        ));
        return;
    };

    let (read, write) = stream.into_split();
    tokio::spawn(writer_task(write, rx));
    reader_task(read, events).await;
}

async fn writer_task(mut write: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        if write.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if write.write_all(b"\n").await.is_err() {
            break;
        }
    }
}

async fn reader_task(read: OwnedReadHalf, events: mpsc::UnboundedSender<MpvEvent>) {
    let mut lines = BufReader::new(read).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                let _ = events.send(MpvEvent::Shutdown);
                return;
            }
            Err(_) => {
                let _ = events.send(MpvEvent::Shutdown);
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let event = v.get("event").and_then(Value::as_str).unwrap_or_default();
        match event {
            "file-loaded" => {
                let _ = events.send(MpvEvent::FileLoaded);
            }
            "end-file" => {
                let reason = v
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("eof")
                    .to_string();
                let _ = events.send(MpvEvent::EndFile { reason });
            }
            "property-change" => {
                let name = v.get("name").and_then(Value::as_str).unwrap_or_default();
                let data = v.get("data");
                match name {
                    "time-pos" => {
                        if let Some(p) = data.and_then(Value::as_f64) {
                            let _ = events.send(MpvEvent::Position(p));
                        }
                    }
                    "duration" => {
                        if let Some(d) = data.and_then(Value::as_f64) {
                            let _ = events.send(MpvEvent::Duration(d));
                        }
                    }
                    "pause" => {
                        if let Some(p) = data.and_then(Value::as_bool) {
                            let _ = events.send(MpvEvent::Pause(p));
                        }
                    }
                    "volume" => {
                        if let Some(vol) = data.and_then(Value::as_f64) {
                            let _ = events.send(MpvEvent::Volume(vol));
                        }
                    }
                    "mute" => {
                        if let Some(m) = data.and_then(Value::as_bool) {
                            let _ = events.send(MpvEvent::Mute(m));
                        }
                    }
                    "idle-active" => {
                        if data.and_then(Value::as_bool) == Some(true) {
                            let _ = events.send(MpvEvent::Idle);
                        }
                    }
                    _ => {}
                }
            }
            "shutdown" => {
                let _ = events.send(MpvEvent::Shutdown);
                return;
            }
            _ => {}
        }
    }
}
