//! Thin socket client: handshake, action writer, server message reader, and
//! auto-spawn of the daemon when nothing is listening.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use melos_proto::{Action, ClientMsg, PROTOCOL_VERSION, ServerMsg, ViewModel, paths};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

pub struct ClientHandle {
    pub rx: mpsc::UnboundedReceiver<ServerMsg>,
    pub tx: mpsc::UnboundedSender<Action>,
}

/// Connect, starting the daemon if needed.
///
/// If `melos.socket` is enabled, the first connection wakes the daemon through
/// systemd. Otherwise the TUI starts a detached child directly. We deliberately
/// do not call `systemctl` on the cold path: for users without the unit it only
/// adds startup latency.
pub async fn connect(spawn: bool) -> Result<(ClientHandle, ViewModel)> {
    let path = paths::socket_path();

    for _ in 0..2 {
        if let Ok(pair) = connect_once(&path).await {
            return Ok(pair);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    if !spawn {
        bail!("melosd is not running");
    }

    spawn_daemon()?;

    for _ in 0..200 {
        if let Ok(pair) = connect_once(&path).await {
            return Ok(pair);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    bail!(
        "could not connect to melosd at {} — try running `melosd` to see the error",
        path.display()
    )
}

/// One connection attempt. Does not spawn anything.
pub async fn probe() -> Result<(ClientHandle, ViewModel)> {
    connect_once(&paths::socket_path()).await
}

async fn connect_once(path: &Path) -> Result<(ClientHandle, ViewModel)> {
    let stream = tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(path))
        .await
        .context("connection timed out")??;
    let (read, mut write) = stream.into_split();

    let hello = ClientMsg::Hello {
        protocol: PROTOCOL_VERSION,
        client: format!("melos-tui/{}", env!("CARGO_PKG_VERSION")),
    };
    let mut line = serde_json::to_string(&hello)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await?;

    let mut lines = BufReader::new(read).lines();
    let first = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .context("daemon handshake timed out")??
        .context("daemon closed the connection during handshake")?;
    let msg: ServerMsg = serde_json::from_str(&first).context("invalid handshake reply")?;

    let snapshot = match msg {
        ServerMsg::Welcome {
            protocol, snapshot, ..
        } => {
            if protocol != PROTOCOL_VERSION {
                bail!("protocol mismatch (daemon {protocol}, client {PROTOCOL_VERSION})");
            }
            snapshot
        }
        ServerMsg::Error { message } => bail!(message),
        other => bail!("unexpected handshake message: {other:?}"),
    };

    let (tx, action_rx) = mpsc::unbounded_channel::<Action>();
    let (msg_tx, rx) = mpsc::unbounded_channel::<ServerMsg>();
    tokio::spawn(client_reader(lines, msg_tx));
    tokio::spawn(client_writer(write, action_rx));

    Ok((ClientHandle { rx, tx }, snapshot))
}

async fn client_reader(
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    tx: mpsc::UnboundedSender<ServerMsg>,
) {
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<ServerMsg>(&line)
                    && tx.send(msg).is_err()
                {
                    return;
                }
            }
            Ok(None) | Err(_) => return,
        }
    }
}

async fn client_writer(
    mut write: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<Action>,
) {
    while let Some(action) = rx.recv().await {
        let msg = ClientMsg::Action { action };
        let Ok(mut line) = serde_json::to_string(&msg) else {
            continue;
        };
        line.push('\n');
        if write.write_all(line.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn spawn_daemon() -> Result<()> {
    let log_path = paths::daemon_log();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let log2 = log.try_clone()?;

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("melosd"));
    }
    candidates.push(PathBuf::from("melosd"));

    for daemon in candidates {
        if daemon.components().count() > 1 && !daemon.exists() {
            continue;
        }
        use std::os::unix::process::CommandExt;
        let spawned = Command::new(&daemon)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log2.try_clone()?)
            .process_group(0)
            .spawn();
        if spawned.is_ok() {
            return Ok(());
        }
    }
    bail!("could not start melosd; install it in PATH or run it manually")
}
