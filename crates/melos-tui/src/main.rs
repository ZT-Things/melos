//! `melos` — the TUI client. Attaches to `melosd`, spawning it if necessary.

mod app;
mod client;
mod keys;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::execute;
use melos_proto::config::Config;
use melos_proto::{Action, paths};
use ratatui_image::picker::cap_parser::QueryStdioOptions;
use ratatui_image::picker::{Picker, ProtocolType};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

use crate::app::{App, View};
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
    Paste(String),
    Resize,
}

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableBracketedPaste);
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

fn tmux_client_is_kitty(client: &str) -> bool {
    client.to_ascii_lowercase().contains("kitty")
}

/// Enable tmux passthrough so the outer terminal can receive Kitty graphics.
fn enable_tmux_passthrough() {
    let _ = Command::new("tmux")
        .args(["set", "-w", "allow-passthrough", "on"])
        .status();
}

fn tmux_client_termname() -> Option<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{client_termname}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn kitty_picker() -> Picker {
    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Kitty);
    picker
}

fn detect_picker() -> Picker {
    let force = std::env::var("MELOS_ART")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let tmux = std::env::var_os("TMUX").is_some();
    if force == "halfblocks" {
        return Picker::halfblocks();
    }
    if force == "kitty" {
        if tmux {
            enable_tmux_passthrough();
        }
        return kitty_picker();
    }
    if tmux {
        // Querying stdio under tmux can steal a keypress; ask tmux instead.
        if tmux_client_termname().is_some_and(|client| tmux_client_is_kitty(&client)) {
            enable_tmux_passthrough();
            return kitty_picker();
        }
        return Picker::halfblocks();
    }
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
    execute!(stdout(), EnableBracketedPaste)?;
    app.set_picker(picker);
    terminal.draw(|frame| ui::draw(frame, &mut app))?;
    let t_first_frame = t0.elapsed();

    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<UiEvent>();
    let input_paused = Arc::new(AtomicBool::new(false));
    let reader_paused = input_paused.clone();
    std::thread::spawn(move || {
        loop {
            if reader_paused.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            match event::read() {
                Ok(Event::Key(key)) => {
                    if key_tx.send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(Event::Paste(text)) => {
                    if key_tx.send(UiEvent::Paste(text)).is_err() {
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
                        if let Some((track_id, initial)) = app.take_editor_request() {
                            input_paused.store(true, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(60));
                            ratatui::restore();
                            let edited = edit_lyrics(&initial);
                            terminal = ratatui::init();
                            execute!(stdout(), EnableBracketedPaste)?;
                            input_paused.store(false, Ordering::Relaxed);
                            terminal.clear()?;
                            if let Some(text) = edited {
                                let _ = action_tx.send(Action::SaveLyrics { track_id, text });
                            }
                            terminal.draw(|frame| ui::draw(frame, &mut app))?;
                        }
                    }
                    Some(UiEvent::Paste(text)) => {
                        app.on_paste(&text);
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
                        let before = app.vm.transport.track.as_ref().map(|t| t.id);
                        app.apply(message);
                        let after = app.vm.transport.track.as_ref().map(|t| t.id);
                        if app.view == View::Lyrics && before != after {
                            let _ = action_tx.send(Action::RequestLyrics { force: false });
                        }
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
            _ = tokio::time::sleep(Duration::from_millis(50)), if app.art_pending() => {
                // The first frame should not wait for image decoding, so poll
                // only while a background decode is outstanding.
                terminal.draw(|frame| ui::draw(frame, &mut app))?;
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

fn find_editor() -> Option<String> {
    for name in ["VISUAL", "EDITOR"] {
        if let Ok(editor) = std::env::var(name)
            && !editor.trim().is_empty()
        {
            return Some(editor);
        }
    }
    ["vi", "vim", "nano"]
        .into_iter()
        .find(|editor| {
            Command::new(editor)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
        })
        .map(str::to_string)
}

fn edit_lyrics(initial: &str) -> Option<String> {
    let path = paths::state_dir().join("lyrics-edit.lrc");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&path, initial).is_err() {
        return None;
    }
    let Some(editor) = find_editor() else {
        let _ = std::fs::remove_file(&path);
        return None;
    };
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status();
    let edited = std::fs::read_to_string(&path)
        .ok()
        .filter(|text| !text.trim().is_empty() && text != initial);
    let _ = std::fs::remove_file(&path);
    edited
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

    #[test]
    fn tmux_outer_terminal_is_detected_as_kitty() {
        assert!(tmux_client_is_kitty("xterm-kitty"));
        assert!(tmux_client_is_kitty("kitty(0.49.2)"));
        assert!(!tmux_client_is_kitty("xterm-256color"));
        assert!(!tmux_client_is_kitty("screen-256color"));
        assert!(!tmux_client_is_kitty(""));
    }
}
