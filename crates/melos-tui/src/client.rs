//! Thin socket client: handshake, action writer, server message reader, and
//! auto-spawn of the daemon when nothing is listening.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use melos_proto::{Action, ClientMsg, PROTOCOL_VERSION, ServerMsg, ViewModel, paths};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

const LEGACY_PROTOCOL: u32 = 1;

#[derive(Debug)]
pub struct ClientHandle {
    pub rx: mpsc::UnboundedReceiver<ServerMsg>,
    pub tx: mpsc::UnboundedSender<Action>,
}

#[derive(Debug)]
struct ProtocolMismatch {
    daemon: Option<u32>,
    client: u32,
    detail: Option<String>,
}

impl ProtocolMismatch {
    fn new(daemon: Option<u32>, client: u32) -> Self {
        Self {
            daemon,
            client,
            detail: None,
        }
    }
}

impl fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(detail) = &self.detail {
            return f.write_str(detail);
        }
        write!(
            f,
            "protocol mismatch (daemon {}, client {})",
            self.daemon
                .map(|d| d.to_string())
                .unwrap_or_else(|| "unknown".into()),
            self.client
        )
    }
}

impl std::error::Error for ProtocolMismatch {}

/// Connect, starting the daemon if needed.
///
/// If `melos.socket` is enabled, the first connection wakes the daemon through
/// systemd. Otherwise the TUI starts a detached child directly. We deliberately
/// do not call `systemctl` on the cold path: for users without the unit it only
/// adds startup latency.
pub async fn connect(spawn: bool) -> Result<(ClientHandle, ViewModel)> {
    let path = paths::socket_path();
    let mut last_error = None;

    for _ in 0..2 {
        match connect_once(&path).await {
            Ok(pair) => return Ok(pair),
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    if !spawn {
        return Err(last_error.unwrap_or_else(|| anyhow::anyhow!("melosd is not running")));
    }

    if last_error
        .as_ref()
        .is_some_and(|error| error.downcast_ref::<ProtocolMismatch>().is_some())
    {
        retire_legacy(&path).await;
    }

    // Socket activation may already own the socket, so a bind error here is
    // not fatal: the retry loop below can still wake the real service.
    let spawn_error = spawn_daemon().err();
    let mut saw_mismatch = false;

    for _ in 0..200 {
        match connect_once(&path).await {
            Ok(pair) => return Ok(pair),
            Err(error) => {
                if error.downcast_ref::<ProtocolMismatch>().is_some() {
                    saw_mismatch = true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    if saw_mismatch {
        bail!(
            "an older melosd (protocol {LEGACY_PROTOCOL}) is still running — run `melos stop`, then start melos again"
        );
    }
    if let Some(error) = spawn_error {
        return Err(error.context(format!("could not connect to melosd at {}", path.display())));
    }
    bail!(
        "could not connect to melosd at {} — try running `melosd` to see the error",
        path.display()
    )
}

/// One connection attempt. Does not spawn anything.
///
/// A daemon from the previous protocol generation is still readable enough to
/// report status and to shut down, which keeps `melos status` and `melos stop`
/// useful during an upgrade.
pub async fn probe() -> Result<(ClientHandle, ViewModel)> {
    let path = paths::socket_path();
    let error = match connect_once(&path).await {
        Ok(pair) => return Ok(pair),
        Err(error) => error,
    };
    if error.downcast_ref::<ProtocolMismatch>().is_some()
        && let Ok(pair) = connect_once_with_protocol(&path, LEGACY_PROTOCOL).await
    {
        return Ok(pair);
    }
    Err(error)
}

/// Ask a protocol-1 daemon to shut down so a freshly installed daemon can take
/// over the socket. Best effort: a failed retirement just falls back to the
/// normal connect error.
async fn retire_legacy(path: &Path) -> bool {
    let Ok((handle, _)) = connect_once_with_protocol(path, LEGACY_PROTOCOL).await else {
        return false;
    };
    let _ = handle.tx.send(Action::Shutdown);
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        match connect_once_with_protocol(path, LEGACY_PROTOCOL).await {
            // Still the old daemon; give mpv and the daemon time to exit.
            Ok(_) => {}
            Err(error) => {
                if error
                    .downcast_ref::<ProtocolMismatch>()
                    .is_some_and(|mismatch| mismatch.daemon == Some(PROTOCOL_VERSION))
                {
                    // Socket activation started the new daemon while the old
                    // listener stayed in place; the upgrade is complete.
                    return true;
                }
                if UnixStream::connect(path).await.is_err() {
                    return true;
                }
            }
        }
    }
    false
}

async fn connect_once(path: &Path) -> Result<(ClientHandle, ViewModel)> {
    connect_once_with_protocol(path, PROTOCOL_VERSION).await
}

async fn connect_once_with_protocol(
    path: &Path,
    protocol: u32,
) -> Result<(ClientHandle, ViewModel)> {
    let stream = tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(path))
        .await
        .context("connection timed out")??;
    let (read, mut write) = stream.into_split();

    let hello = ClientMsg::Hello {
        protocol,
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
            protocol: daemon_protocol,
            snapshot,
            ..
        } => {
            if daemon_protocol != protocol {
                return Err(ProtocolMismatch::new(Some(daemon_protocol), protocol).into());
            }
            snapshot
        }
        ServerMsg::Error { message } => {
            if message.starts_with("protocol mismatch") {
                let daemon = message
                    .split("daemon ")
                    .nth(1)
                    .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|digits| digits.parse().ok());
                return Err(ProtocolMismatch {
                    daemon,
                    client: protocol,
                    detail: Some(message),
                }
                .into());
            }
            bail!(message)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn protocol_mismatch_is_a_typed_error() {
        let path = std::env::temp_dir().join(format!(
            "melos-client-test-welcome-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let _ = lines.next_line().await.unwrap();
            let reply = serde_json::to_string(&ServerMsg::Welcome {
                protocol: 99,
                daemon_version: "test".into(),
                snapshot: ViewModel::default(),
            })
            .unwrap();
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            write.flush().await.unwrap();
        });

        let err = connect_once_with_protocol(&path, PROTOCOL_VERSION)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<ProtocolMismatch>().is_some());
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn retire_waits_for_a_slow_legacy_daemon_to_exit() {
        let path = std::env::temp_dir().join(format!(
            "melos-client-test-retire-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let hello = lines.next_line().await.unwrap().unwrap();
            let hello: ClientMsg = serde_json::from_str(&hello).unwrap();
            assert!(matches!(hello, ClientMsg::Hello { protocol: 1, .. }));
            let reply = serde_json::to_string(&ServerMsg::Welcome {
                protocol: 1,
                daemon_version: "0.1.0".into(),
                snapshot: ViewModel::default(),
            })
            .unwrap();
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            write.flush().await.unwrap();
            let action = lines.next_line().await.unwrap().unwrap();
            let action: ClientMsg = serde_json::from_str(&action).unwrap();
            assert!(matches!(
                action,
                ClientMsg::Action {
                    action: Action::Shutdown
                }
            ));
            tokio::time::sleep(Duration::from_millis(1500)).await;
            drop(listener);
        });

        assert!(retire_legacy(&path).await);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn protocol_mismatch_error_reply_is_typed() {
        let path = std::env::temp_dir().join(format!(
            "melos-client-test-error-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let _ = lines.next_line().await.unwrap();
            let reply = serde_json::to_string(&ServerMsg::Error {
                message: "protocol mismatch (daemon 1, client 2)".into(),
            })
            .unwrap();
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            write.flush().await.unwrap();
        });

        let err = connect_once_with_protocol(&path, PROTOCOL_VERSION)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<ProtocolMismatch>().is_some());
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
