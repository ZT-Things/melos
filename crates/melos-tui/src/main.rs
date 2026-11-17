//! `melos` — the TUI client. Attaches to `melosd`, spawning it if necessary.

mod app;
mod client;
mod keys;
mod ui;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event};
use melos_proto::Action;
use melos_proto::config::Config;
use ratatui_image::picker::Picker;
use ratatui_image::picker::cap_parser::QueryStdioOptions;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

use crate::app::App;
use crate::client::ClientHandle;
use crate::keys::KeyMap;

#[derive(Parser, Debug)]
#[command(
    name = "melos",
    version,
    about = "Fast, minimal TUI music player with YouTube downloads"
)]
struct Args {
    /// Path to config.toml (defaults to $XDG_CONFIG_HOME/melos/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Print startup timings after the UI exits
    #[arg(long)]
    trace_startup: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show whether the daemon is running
    Status,
    /// Stop playback and shut down the daemon
    Stop,
}

enum UiEvent {
    Key(crossterm::event::KeyEvent),
    Resize,
}

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let t0 = Instant::now();
    let args = Args::parse();
    match args.cmd {
        Some(Cmd::Status) => return status().await,
        Some(Cmd::Stop) => return stop().await,
        None => {}
    }
    run_tui(args, t0).await
}

async fn status() -> Result<()> {
    match client::probe().await {
        Ok((_handle, vm)) => {
            println!(
                "melosd {} running (protocol {})",
                vm.daemon_version, vm.protocol
            );
            println!(
                "{} tracks · {} in queue · {}",
                vm.tracks.len(),
                vm.queue.len(),
                if vm.transport.playing {
                    "playing"
                } else if vm.transport.track.is_some() {
                    "paused"
                } else {
                    "idle"
                }
            );
            if let Some(track) = &vm.transport.track {
                println!("now: {} — {}", track.title, track.artist);
            }
        }
        Err(_) => println!("melosd is not running"),
    }
    Ok(())
}

async fn stop() -> Result<()> {
    match client::probe().await {
        Ok((handle, _)) => {
            let _ = handle.tx.send(Action::Shutdown);
            sleep(Duration::from_millis(150)).await;
            println!("melosd stopped");
        }
        Err(_) => println!("melosd is not running"),
    }
    Ok(())
}

/// Terminal stdio queries can consume a keypress and briefly leave raw mode, so
/// only probe terminals where Kitty graphics are actually likely. tmux uses the
/// reliable halfblock fallback instead.
fn wants_graphics_query(
    tmux: bool,
    term: Option<&str>,
    term_program: Option<&str>,
    kitty_window: Option<&str>,
) -> bool {
    if tmux {
        return false;
    }
    term.is_some_and(|value| value.contains("kitty"))
        || term_program.is_some_and(|value| value.eq_ignore_ascii_case("kitty"))
        || kitty_window.is_some()
}

fn detect_picker() -> Picker {
    let tmux = std::env::var_os("TMUX").is_some();
    let term = std::env::var("TERM").ok();
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let kitty_window = std::env::var("KITTY_WINDOW_ID").ok();
    if wants_graphics_query(
        tmux,
        term.as_deref(),
        term_program.as_deref(),
        kitty_window.as_deref(),
    ) {
        Picker::from_query_stdio_with_options(QueryStdioOptions {
            timeout: Duration::from_millis(500),
            ..Default::default()
        })
        .unwrap_or_else(|_| Picker::halfblocks())
    } else {
        Picker::halfblocks()
    }
}

async fn run_tui(args: Args, t0: Instant) -> Result<()> {
    let cfg = match Config::load(args.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("melos: {e}; using defaults");
            Config::default()
        }
    };
    let t_cfg = t0.elapsed();

    let (client, snapshot) = client::connect(true).await?;
    let t_conn = t0.elapsed();

    let mut app = App::new(snapshot, KeyMap::from_config(&cfg.keys));

    let picker = detect_picker();
    let mut terminal = ratatui::init();
    let guard = Restore;
    app.set_picker(picker);
    terminal.draw(|frame| ui::draw(frame, &mut app))?;
    let t_first_frame = t0.elapsed();

    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<UiEvent>();
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key)) => {
                    if key_tx.send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    if key_tx.send(UiEvent::Resize).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let ClientHandle {
        rx: mut server_rx,
        tx: action_tx,
    } = client;

    loop {
        tokio::select! {
            maybe = key_rx.recv() => {
                match maybe {
                    Some(UiEvent::Key(key)) => {
                        let actions = app.on_key(key);
                        for action in actions {
                            let _ = action_tx.send(action);
                        }
                        if app.quit {
                            break;
                        }
                        terminal.draw(|frame| ui::draw(frame, &mut app))?;
                    }
                    Some(UiEvent::Resize) => {
                        terminal.draw(|frame| ui::draw(frame, &mut app))?;
                    }
                    None => break,
                }
            }
            message = server_rx.recv() => {
                match message {
                    Some(message) => {
                        app.apply(message);
                        if app.quit {
                            break;
                        }
                        terminal.draw(|frame| ui::draw(frame, &mut app))?;
                    }
                    None => {
                        app.daemon_lost = true;
                        app.status_override = Some("lost connection to melosd".into());
                        break;
                    }
                }
            }
        }
    }

    drop(guard);

    if app.daemon_lost {
        eprintln!("melos: {}", app.status_text());
    }
    if args.trace_startup {
        eprintln!(
            "melos: first frame {:.1} ms (config {:.1} ms, daemon {:.1} ms, draw {:.1} ms)",
            t_first_frame.as_secs_f64() * 1000.0,
            t_cfg.as_secs_f64() * 1000.0,
            t_conn.as_secs_f64() * 1000.0,
            (t_first_frame - t_conn).as_secs_f64() * 1000.0,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphics_query_is_only_used_for_direct_kitty_terminals() {
        assert!(wants_graphics_query(
            false,
            Some("xterm-kitty"),
            Some("kitty"),
            None
        ));
        assert!(!wants_graphics_query(
            true,
            Some("xterm-kitty"),
            Some("kitty"),
            None
        ));
        assert!(!wants_graphics_query(
            false,
            Some("xterm-256color"),
            Some("tmux"),
            None
        ));
    }
}
