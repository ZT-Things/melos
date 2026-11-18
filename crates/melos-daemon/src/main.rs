//! `melosd` — the resident daemon. Owns playback, the library, downloads and
//! (eventually) MPRIS. The TUI is a thin client over the unix socket.

mod art;
mod db;
mod download;
mod lyrics;
mod mpris;
mod mpv;
mod player;
mod state;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use melos_proto::{Action, ClientMsg, PROTOCOL_VERSION, ServerMsg, config::Config, paths};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;

use crate::db::Db;
use crate::mpv::{MpvEvent, MpvHandle};
use crate::player::Player;
use crate::state::{Control, Daemon, Internal};

#[derive(Parser, Debug)]
#[command(name = "melosd", version, about = "melos resident daemon")]
struct Args {
    /// Path to config.toml (defaults to $XDG_CONFIG_HOME/melos/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override the music directory
    #[arg(long)]
    music_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("melosd=info")),
        )
        .with_ansi(false)
        .with_target(false)
        .init();

    let mut cfg = Config::load(args.config.as_deref())?;
    if let Some(dir) = args.music_dir {
        cfg.music_dir = dir.to_string_lossy().to_string();
    }
    cfg.normalize();
    let cfg = Arc::new(cfg);

    std::fs::create_dir_all(paths::data_dir()).context("creating data directory")?;
    std::fs::create_dir_all(paths::state_dir()).context("creating state directory")?;
    std::fs::create_dir_all(paths::runtime_dir()).context("creating runtime directory")?;

    let (listener, owned_socket) = bind_listener()?;

    let db = Db::open(&paths::db_path())?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Internal>();

    // Kill a stale mpv left behind by a crashed daemon.
    kill_orphan_mpv();

    let mpv = match MpvHandle::spawn(&paths::mpv_socket_path(), &cfg.mpv.extra_args, {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<MpvEvent>();
        let forward = tx.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if forward.send(Internal::Mpv(event)).is_err() {
                    break;
                }
            }
        });
        event_tx
    }) {
        Ok(handle) => {
            tracing::info!("mpv starting");
            Some(handle)
        }
        Err(e) => {
            tracing::warn!("playback unavailable: {e}");
            None
        }
    };

    let player = Player::new(mpv, 70, false, Default::default());
    let mut daemon = Daemon::new(cfg.clone(), db, player, tx.clone());
    daemon.set_semaphore(Arc::new(Semaphore::new(cfg.concurrent_downloads)));

    // D-Bus activation is not on the critical path; the TUI can connect while
    // MPRIS is still being registered.
    tokio::spawn({
        let tx = tx.clone();
        async move {
            match mpris::start(tx.clone()).await {
                Ok((server, info)) => {
                    tracing::info!("MPRIS ready");
                    let _ = tx.send(Internal::Mpris(server, info));
                }
                Err(e) => tracing::warn!("MPRIS unavailable: {e}"),
            }
        }
    });

    tokio::spawn({
        let tx = tx.clone();
        async move {
            if let Some(age) = download::yt_dlp_age_days().await
                && age > 90
            {
                let _ = tx.send(Internal::Note(format!(
                    "yt-dlp is {age} days old — update it if downloads fail"
                )));
            }
        }
    });

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tracing::info!("listening on {}", paths::socket_path().display());

    let mut shutdown = false;
    while !shutdown {
        tokio::select! {
            maybe = rx.recv() => {
                match maybe {
                    Some(internal) => {
                        if let Control::Shutdown = daemon.handle(internal).await {
                            shutdown = true;
                        }
                    }
                    None => shutdown = true,
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        tokio::spawn(client_session(stream, tx.clone()));
                    }
                    Err(e) => tracing::warn!("accept failed: {e}"),
                }
            }
            _ = tick.tick() => {
                let _ = daemon.handle(Internal::Tick).await;
            }
            _ = sigterm.recv() => shutdown = true,
            _ = sigint.recv() => shutdown = true,
        }
    }

    daemon.announce_shutdown();
    daemon.shutdown().await;
    if let Some(path) = owned_socket {
        let _ = std::fs::remove_file(path);
    }
    tracing::info!("stopped");
    Ok(())
}

/// Use the systemd-activated socket when present, otherwise bind our own.
fn bind_listener() -> Result<(UnixListener, Option<PathBuf>)> {
    let activated = std::env::var("LISTEN_FDS").ok().as_deref() == Some("1")
        && std::env::var("LISTEN_PID")
            .ok()
            .and_then(|p| p.parse::<u32>().ok())
            == Some(std::process::id());

    if activated {
        use std::os::fd::FromRawFd;
        // SAFETY: systemd passes exactly one listening socket on fd 3 when
        // LISTEN_FDS=1 and LISTEN_PID matches this process.
        let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
        std_listener.set_nonblocking(true)?;
        let listener = UnixListener::from_std(std_listener)?;
        tracing::info!("using systemd socket activation");
        return Ok((listener, None));
    }

    let path = paths::socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            anyhow::bail!("melosd is already running");
        }
        let _ = std::fs::remove_file(&path);
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, Some(path)))
}

fn kill_orphan_mpv() {
    let socket = paths::mpv_socket_path();
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket) {
        use std::io::Write;
        let _ = stream.write_all(b"{\"command\":[\"quit\"]}\n");
        let _ = stream.flush();
    }
    let _ = std::fs::remove_file(socket);
}

async fn client_session(stream: UnixStream, tx: mpsc::UnboundedSender<Internal>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    let hello = match tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await {
        Ok(Ok(Some(line))) => line,
        _ => return,
    };
    let hello: ClientMsg = match serde_json::from_str(&hello) {
        Ok(msg) => msg,
        Err(_) => return,
    };

    match hello {
        ClientMsg::Hello { protocol, .. } if protocol == PROTOCOL_VERSION => {}
        ClientMsg::Hello { protocol, .. } => {
            let msg = ServerMsg::Error {
                message: format!(
                    "protocol mismatch: client {protocol}, daemon {PROTOCOL_VERSION}. Restart melosd."
                ),
            };
            let _ = write_line(&mut write, &msg).await;
            return;
        }
        ClientMsg::Action { .. } => return,
    }

    let (ctx, crx) = mpsc::unbounded_channel::<ServerMsg>();
    if tx.send(Internal::Client(ctx)).is_err() {
        return;
    }
    tokio::spawn(client_writer(write, crx));

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if let Ok(ClientMsg::Action { action }) = serde_json::from_str::<ClientMsg>(&line)
                    && tx.send(Internal::Action(action)).is_err()
                {
                    return;
                }
            }
            Ok(None) => return,
            Err(_) => return,
        }
    }
}

async fn client_writer(mut write: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<ServerMsg>) {
    while let Some(msg) = rx.recv().await {
        let stop = matches!(msg, ServerMsg::Shutdown);
        if write_line(&mut write, &msg).await.is_err() {
            return;
        }
        if stop {
            return;
        }
    }
}

async fn write_line(write: &mut OwnedWriteHalf, msg: &ServerMsg) -> std::io::Result<()> {
    let mut line = serde_json::to_string(msg).map_err(std::io::Error::other)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await
}

/// Keep `Action` imported for the protocol docs; referenced in tests.
#[allow(dead_code)]
fn _action_type_check(_: Action) {}
