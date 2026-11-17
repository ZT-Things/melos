//! Rendering. Everything is derived from `App`; no daemon state lives here.

use melos_proto::{DownloadState, Repeat, format_duration, truncate};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Focus, InputKind, Overlay, View};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const NOW: Color = Color::Green;

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.clamp_cursor();
    let area = frame.area();
    let [body, now, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(24), Constraint::Min(20)]).areas(body);

    draw_sidebar(frame, app, sidebar);
    draw_main(frame, app, main);
    draw_now_playing(frame, app, now);
    draw_status(frame, app, status);

    if let Some(overlay) = &app.overlay {
        match overlay {
            Overlay::Help => draw_help(frame, app, area),
            Overlay::PlaylistPicker { index, .. } => draw_picker(frame, app, area, *index),
        }
    }
    if let Some(input) = &app.input {
        draw_input(frame, app, input.kind, &input.buffer, status);
    }
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(Line::from(vec![
        Span::styled(
            " melos ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("v{} ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(DIM),
        ),
    ]));
    let labels = app.sidebar_labels();
    let items: Vec<ListItem> = labels
        .iter()
        .map(|(label, count)| {
            ListItem::new(Line::from(vec![
                Span::raw(label.clone()),
                Span::styled(format!("  {count}"), Style::default().fg(DIM)),
            ]))
        })
        .collect();
    let selected = if app.focus == Focus::Sidebar {
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT)
    };
    let list = List::new(items)
        .block(block)
        .highlight_style(selected)
        .highlight_symbol("▸ ");
    let mut state = ListState::default().with_selected(Some(app.sidebar_idx));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_main(frame: &mut Frame, app: &App, area: Rect) {
    let mut title = match app.view {
        View::PlaylistTracks => {
            let name = app
                .vm
                .selected_playlist
                .and_then(|id| app.vm.playlists.iter().find(|p| p.id == id))
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Playlist".into());
            format!(" {name} ")
        }
        other => format!(" {} ", other.title()),
    };
    if !app.filter.is_empty() {
        title.push_str(&format!("filter: {}  ", truncate(&app.filter, 30)));
    }
    let block = Block::bordered().title(Span::styled(
        title,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    let width = inner.width.saturating_sub(4) as usize;
    let items = match app.view {
        View::Library | View::PlaylistTracks => track_items(app, width),
        View::Playlists => playlist_items(app, width),
        View::Queue => queue_items(app, width),
        View::Downloads => download_items(app, width),
    };

    if items.is_empty() {
        let empty = match app.view {
            View::Library => {
                "No tracks yet. Press i, paste a YouTube URL or search — Enter downloads."
            }
            View::Playlists => "No playlists yet. Press N to create one.",
            View::PlaylistTracks => {
                "This playlist is empty. Press A on a track in the library to add it."
            }
            View::Queue => "The queue is empty. Press a on a track to add it.",
            View::Downloads => "Press i to search YouTube or paste a URL.",
        };
        let p = Paragraph::new(empty)
            .style(Style::default().fg(DIM))
            .block(block)
            .wrap(Wrap { trim: true });
        frame.render_widget(p, area);
        return;
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(if app.focus == Focus::Main {
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        })
        .highlight_symbol("▸ ");
    let mut state = ListState::default().with_selected(Some(app.main_idx));
    frame.render_stateful_widget(list, area, &mut state);
}

fn track_items(app: &App, width: usize) -> Vec<ListItem<'static>> {
    let playing_id = app.vm.transport.track.as_ref().map(|t| t.id);
    app.visible_tracks()
        .iter()
        .map(|t| {
            let playing = playing_id == Some(t.id);
            let marker = if playing { "▶" } else { " " };
            let marker_style = if playing {
                Style::default().fg(NOW)
            } else {
                Style::default()
            };
            let title_style = if t.missing {
                Style::default().fg(Color::Red)
            } else if playing {
                Style::default().fg(NOW).add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let title_width = width.saturating_sub(30).max(10);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} "), marker_style),
                Span::styled(truncate(&t.title, title_width), title_style),
                Span::raw("  "),
                Span::styled(truncate(&t.artist, 24), Style::default().fg(DIM)),
                Span::raw("  "),
                Span::styled(t.duration_label(), Style::default().fg(DIM)),
            ]))
        })
        .collect()
}

fn playlist_items(app: &App, width: usize) -> Vec<ListItem<'static>> {
    app.vm
        .playlists
        .iter()
        .map(|p| {
            ListItem::new(Line::from(vec![
                Span::styled(truncate(&p.name, width.saturating_sub(8)), Style::default()),
                Span::styled(format!("  {}", p.count), Style::default().fg(DIM)),
            ]))
        })
        .collect()
}

fn queue_items(app: &App, width: usize) -> Vec<ListItem<'static>> {
    app.vm
        .queue
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let current = app.vm.queue_pos == Some(i);
            let marker = if current { "▶" } else { " " };
            let style = if current {
                Style::default().fg(NOW)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::styled(format!("{:>3} ", i + 1), Style::default().fg(DIM)),
                Span::styled(truncate(&t.title, width.saturating_sub(30)), style),
                Span::raw("  "),
                Span::styled(truncate(&t.artist, 20), Style::default().fg(DIM)),
            ]))
        })
        .collect()
}

fn download_items(app: &App, width: usize) -> Vec<ListItem<'static>> {
    let mut items = Vec::new();
    for r in &app.vm.yt_results {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("↓ ", Style::default().fg(ACCENT)),
            Span::styled(
                truncate(&r.title, width.saturating_sub(30)),
                Style::default(),
            ),
            Span::raw("  "),
            Span::styled(truncate(&r.uploader, 18), Style::default().fg(DIM)),
            Span::raw("  "),
            Span::styled(r.duration_label(), Style::default().fg(DIM)),
        ])));
    }
    for d in &app.vm.downloads {
        let (marker, style) = match d.state {
            DownloadState::Done => ("✓ ", Style::default().fg(NOW)),
            DownloadState::Failed => ("✗ ", Style::default().fg(Color::Red)),
            DownloadState::Cancelled => ("– ", Style::default().fg(DIM)),
            _ => ("• ", Style::default().fg(ACCENT)),
        };
        let detail = if d.state == DownloadState::Downloading {
            format!("{:>3.0}%  {}", d.percent, truncate(&d.detail, 24))
        } else if let Some(err) = &d.error {
            truncate(err, 40)
        } else {
            d.state.label().to_string()
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(truncate(&d.title, width.saturating_sub(34)), style),
            Span::raw("  "),
            Span::styled(detail, Style::default().fg(DIM)),
        ])));
    }
    items
}

fn draw_now_playing(frame: &mut Frame, app: &App, area: Rect) {
    let [line_area, gauge_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let t = &app.vm.transport;

    let (left, right) = match &t.track {
        Some(track) => {
            let icon = if t.playing { "▶" } else { "❚❚" };
            let left = format!("{icon} {} — {}", track.title, track.artist);
            let right = format!(
                "{} / {}   vol {}{}",
                format_duration(t.position_ms),
                format_duration(t.duration_ms.max(track.duration_ms)),
                t.volume,
                if t.muted { "% (muted)" } else { "%" },
            );
            (left, right)
        }
        None => (
            "nothing playing — Enter on a track to start".to_string(),
            format!("vol {}%", t.volume),
        ),
    };
    let width = line_area.width as usize;
    let right = fit(&right, width.saturating_sub(12).max(1));
    let right_width = right.width();
    let left_max = width.saturating_sub(right_width + 1).max(8);
    let left = fit(&left, left_max);
    let line = Line::from(vec![
        Span::styled(left.clone(), Style::default().fg(NOW)),
        Span::raw(" ".repeat(width.saturating_sub(left.width() + right_width))),
        Span::styled(right, Style::default().fg(DIM)),
    ]);
    frame.render_widget(Paragraph::new(line), line_area);

    let duration = t.duration_ms.max(1);
    let ratio = (t.position_ms as f64 / duration as f64).clamp(0.0, 1.0);
    let gauge = Gauge::default()
        .gauge_style(
            Style::default()
                .fg(if t.playing { NOW } else { DIM })
                .bg(Color::Black),
        )
        .ratio(ratio)
        .label("");
    frame.render_widget(gauge, gauge_area);

    let hints = "j/k move  Enter play  Space pause  n/p next/prev  a queue  A playlist  / filter  i download  ? help  q detach";
    frame.render_widget(
        Paragraph::new(truncate(hints, hint_area.width as usize)).style(Style::default().fg(DIM)),
        hint_area,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let left = app.status_text();
    let modes = format!(
        "shuffle {}   repeat {}",
        if app.vm.transport.shuffle {
            "on"
        } else {
            "off"
        },
        match app.vm.transport.repeat {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        },
    );
    let width = area.width as usize;
    let modes = fit(&modes, width.saturating_sub(10));
    let left_max = width.saturating_sub(modes.width() + 1).max(10);
    let left = fit(&left, left_max);
    let gap = width.saturating_sub(left.width() + modes.width());
    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(Color::Gray)),
        Span::raw(" ".repeat(gap)),
        Span::styled(modes, Style::default().fg(DIM)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_input(frame: &mut Frame, app: &App, kind: InputKind, buffer: &str, area: Rect) {
    let (label, hint) = match kind {
        InputKind::Filter => ("filter", "Enter apply · Esc clear"),
        InputKind::NewPlaylist => ("new playlist", "Enter create · Esc cancel"),
        InputKind::Download => ("search / url", "Enter search or download · Esc cancel"),
    };
    let text = format!(" {label}: {buffer}▏  {hint}");
    frame.render_widget(
        Paragraph::new(truncate(&text, area.width as usize))
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
        area,
    );
    let _ = app;
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.keys.help_rows();
    let half = rows.len().div_ceil(2);
    let width = area.width.saturating_sub(6).min(82).max(40);
    let height = (half as u16 + 4).min(area.height);
    let popup = centered_abs(width, height, area);
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(Span::styled(
        " Keys — press any key to close ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    let mut lines: Vec<Line> = Vec::new();
    for i in 0..half {
        let mut spans = Vec::new();
        if let Some((action, chords)) = rows.get(i) {
            spans.push(Span::styled(
                format!("{:<19}", truncate(&friendly(action), 19)),
                Style::default().fg(Color::White),
            ));
            spans.push(Span::styled(
                format!("{:<20}", truncate(chords, 19)),
                Style::default().fg(ACCENT),
            ));
        } else {
            spans.push(Span::raw(" ".repeat(39)));
        }
        if let Some((action, chords)) = rows.get(i + half) {
            spans.push(Span::styled(
                format!("{:<19}", truncate(&friendly(action), 19)),
                Style::default().fg(Color::White),
            ));
            spans.push(Span::styled(
                truncate(chords, 19),
                Style::default().fg(ACCENT),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Remap in ~/.config/melos/config.toml · q detaches, Q stops the daemon",
        Style::default().fg(DIM),
    )));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn fit(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let limit = max - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > limit {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn centered_abs(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn draw_picker(frame: &mut Frame, app: &App, area: Rect, index: usize) {
    let popup = centered(44, 50, area);
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = app
        .vm
        .playlists
        .iter()
        .map(|p| ListItem::new(format!("{}  ({})", p.name, p.count)))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" Add to playlist "))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    let mut state = ListState::default().with_selected(Some(index));
    frame.render_stateful_widget(list, popup, &mut state);
}

fn centered(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ]);
    let [_, middle, _] = vertical.areas(area);
    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ]);
    let [_, center, _] = horizontal.areas(middle);
    center
}

fn friendly(action: &str) -> String {
    match action {
        "down" => "move down",
        "up" => "move up",
        "top" => "go top",
        "bottom" => "go bottom",
        "half_down" => "half page down",
        "half_up" => "half page up",
        "page_down" => "page down",
        "page_up" => "page up",
        "tab" => "switch pane",
        "pane_library" => "library pane",
        "pane_playlists" => "playlists pane",
        "pane_queue" => "queue pane",
        "pane_downloads" => "downloads pane",
        "confirm" => "play / open",
        "pause" => "play-pause",
        "next" => "next track",
        "prev" => "prev track",
        "seek_back" => "seek -5s",
        "seek_fwd" => "seek +5s",
        "seek_back_long" => "seek -30s",
        "seek_fwd_long" => "seek +30s",
        "volume_up" => "volume up",
        "volume_down" => "volume down",
        "mute" => "mute",
        "shuffle" => "shuffle",
        "repeat" => "repeat mode",
        "queue_add" => "add to queue",
        "playlist_add" => "add to playlist",
        "new_playlist" => "new playlist",
        "remove" => "remove / cancel",
        "filter" => "filter",
        "download" => "download / search",
        "playlist_delete" => "delete playlist",
        "help" => "help",
        "cancel" => "back / clear",
        "quit" => "detach UI",
        "shutdown" => "stop daemon",
        other => other,
    }
    .to_string()
}
