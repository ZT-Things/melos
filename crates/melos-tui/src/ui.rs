//! Rendering. Everything is derived from `App`; no daemon state lives here.

use melos_proto::lyrics::{current_line, parse_lrc};
use melos_proto::{DownloadState, Repeat, format_duration, truncate};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui_image::{FontSize, Resize, StatefulImage};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, DeleteChoice, Focus, InputKind, Overlay, View};

const ACCENT: Color = Color::Rgb(214, 168, 95);
const NOW: Color = Color::Rgb(157, 191, 143);
const DIM: Color = Color::Rgb(122, 116, 106);
const SURFACE: Color = Color::Rgb(48, 44, 40);
const VISUAL: Color = Color::Rgb(66, 57, 46);
const DANGER: Color = Color::Rgb(207, 106, 94);

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.poll_art();
    app.clamp_cursor();
    let area = frame.area();
    let [body, now, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .areas(area);
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(20)]).areas(body);

    draw_sidebar(frame, app, sidebar);
    draw_main(frame, app, main);
    draw_now_playing(frame, app, now);
    draw_status(frame, app, status);

    if let Some(overlay) = &app.overlay {
        match overlay {
            Overlay::Help => draw_help(frame, app, area),
            Overlay::PlaylistPicker { index, .. } => draw_picker(frame, app, area, *index),
            Overlay::DeleteTrack { title, choice, .. } => {
                draw_delete(frame, area, title, *choice);
            }
            Overlay::Sources { index } => draw_sources(frame, app, area, *index),
            Overlay::ImportChoice { path, copy } => {
                draw_import_choice(frame, area, path, *copy);
            }
        }
    }
    if let Some(input) = &app.input {
        match &input.kind {
            InputKind::LyricsPaste(track_id) => {
                draw_lyrics_paste(frame, app, *track_id, &input.buffer, area)
            }
            kind => draw_input(frame, app, kind, &input.buffer, status),
        }
    }
}

fn draw_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            Span::styled(
                " melos ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{} ", env!("CARGO_PKG_VERSION")),
                Style::default().fg(DIM),
            ),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let art_width = inner.width.saturating_sub(2).max(1);
    let font = app
        .picker
        .as_ref()
        .map(|picker| picker.font_size())
        .unwrap_or(FontSize::new(10, 20));
    let art_height = if inner.height >= 16 {
        let max = inner.height.saturating_sub(5).clamp(4, 12);
        art_height_for_aspect(app.art_aspect().unwrap_or(9.0 / 16.0), art_width, font, max)
    } else {
        0
    };
    let [art_area, meta_area, nav_area] = Layout::vertical([
        Constraint::Length(art_height),
        Constraint::Length(2),
        Constraint::Min(3),
    ])
    .areas(inner);

    if let Some(art) = app.art.as_mut() {
        let image_area = Rect {
            x: art_area.x.saturating_add(1),
            width: art_area.width.saturating_sub(2),
            ..art_area
        };
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Fit(None)),
            image_area,
            art,
        );
    } else {
        frame.render_widget(
            Paragraph::new("♪")
                .alignment(Alignment::Center)
                .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            art_area,
        );
    }

    let (title, artist) = match &app.vm.transport.track {
        Some(track) => (
            track.title.clone(),
            if track.artist.is_empty() {
                "unknown artist".to_string()
            } else {
                track.artist.clone()
            },
        ),
        None => (
            "Nothing playing".to_string(),
            "press i to find music".to_string(),
        ),
    };
    let meta_width = meta_area.width.saturating_sub(2) as usize;
    let meta = Paragraph::new(vec![
        Line::from(Span::styled(
            fit(&title, meta_width),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            fit(&artist, meta_width),
            Style::default().fg(DIM),
        )),
    ]);
    frame.render_widget(meta, meta_area);

    let labels = app.sidebar_labels();
    let items: Vec<ListItem> = labels
        .iter()
        .map(|(label, count)| {
            let mut spans = vec![Span::raw(label.clone())];
            if *count > 0 {
                spans.push(Span::styled(format!("  {count}"), Style::default().fg(DIM)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let selected = if app.focus == Focus::Sidebar {
        Style::default()
            .bg(SURFACE)
            .fg(Color::Reset)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT)
    };
    let list = List::new(items)
        .highlight_style(selected)
        .highlight_symbol("▸ ");
    let mut state = ListState::default().with_selected(Some(app.sidebar_idx));
    frame.render_stateful_widget(list, nav_area, &mut state);
}

fn draw_main(frame: &mut Frame, app: &mut App, area: Rect) {
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
    if app.visual_anchor.is_some() {
        title.push_str("VISUAL  ");
    }
    if !app.filter.is_empty() {
        title.push_str(&format!("filter: {}  ", truncate(&app.filter, 30)));
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Reset)
                .add_modifier(Modifier::BOLD),
        ));
    if app.view == View::Lyrics {
        draw_lyrics(frame, app, block, area);
        return;
    }
    if app.view == View::Stats {
        draw_stats(frame, app, block, area);
        return;
    }
    let inner = block.inner(area);
    let width = inner.width.saturating_sub(4) as usize;
    let items = match app.view {
        View::Library | View::PlaylistTracks => track_items(app, width),
        View::Playlists => playlist_items(app, width),
        View::Queue => queue_items(app, width),
        View::Downloads => download_items(app, width),
        View::Lyrics | View::Stats => Vec::new(),
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
            View::Lyrics => "No lyrics yet.",
            View::Stats => "No listening time yet.",
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
                .bg(SURFACE)
                .fg(Color::Reset)
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
    let visual = app.visual_range();
    app.visible_tracks()
        .iter()
        .enumerate()
        .map(|(index, t)| {
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
            let line = Line::from(vec![
                Span::styled(format!("{marker} "), marker_style),
                Span::styled(truncate(&t.title, title_width), title_style),
                Span::raw("  "),
                Span::styled(truncate(&t.artist, 24), Style::default().fg(DIM)),
                Span::raw("  "),
                Span::styled(t.duration_label(), Style::default().fg(DIM)),
            ]);
            let line = if visual.is_some_and(|(start, end)| index >= start && index <= end) {
                line.style(Style::default().bg(VISUAL))
            } else {
                line
            };
            ListItem::new(line)
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
    let visual = app.visual_range();
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
            let line = Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::styled(format!("{:>3} ", i + 1), Style::default().fg(DIM)),
                Span::styled(truncate(&t.title, width.saturating_sub(30)), style),
                Span::raw("  "),
                Span::styled(truncate(&t.artist, 20), Style::default().fg(DIM)),
            ]);
            let line = if visual.is_some_and(|(start, end)| i >= start && i <= end) {
                line.style(Style::default().bg(VISUAL))
            } else {
                line
            };
            ListItem::new(line)
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

fn draw_lyrics(frame: &mut Frame, app: &mut App, block: Block<'_>, area: Rect) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(inner);
    app.lyrics_page = body.height.max(1) as usize;

    let Some(track) = app.vm.transport.track.as_ref() else {
        frame.render_widget(
            Paragraph::new("Play a track to load its lyrics.").style(Style::default().fg(DIM)),
            body,
        );
        return;
    };

    let source = if app.vm.lyrics_loading {
        "LRCLIB · searching".to_string()
    } else if let Some(lyrics) = &app.vm.lyrics {
        let kind = if lyrics.synced.is_some() {
            "synced"
        } else {
            "plain"
        };
        format!("{} · {kind}", lyrics.source)
    } else if app.vm.lyrics_error.is_some() {
        "no lyrics".to_string()
    } else {
        String::new()
    };
    let width = header.width as usize;
    let title = format!("{} — {}", track.title, track.artist);
    let title = fit(&title, width.saturating_sub(source.width() + 1));
    let gap = width.saturating_sub(title.width() + source.width());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                title,
                Style::default()
                    .fg(Color::Reset)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(gap)),
            Span::styled(source, Style::default().fg(DIM)),
        ])),
        Rect {
            height: 1,
            ..header
        },
    );

    let offset = app.vm.lyrics.as_ref().map(|l| l.offset_ms).unwrap_or(0);
    let synced = app
        .vm
        .lyrics
        .as_ref()
        .and_then(|lyrics| lyrics.synced.as_ref())
        .is_some();
    let mut footer_text = if synced {
        "j/k select · Ctrl+d/u half page · PgUp/PgDn · Enter seek · p/e/f · [ ] offset".to_string()
    } else {
        "j/k scroll · Ctrl+d/u half page · PgUp/PgDn · p/e/f · [ ] offset".to_string()
    };
    if offset != 0 {
        footer_text.push_str(&format!(" ({offset:+}ms)"));
    }
    frame.render_widget(
        Paragraph::new(fit(&footer_text, footer.width as usize)).style(Style::default().fg(DIM)),
        footer,
    );

    if app.vm.lyrics_loading {
        frame.render_widget(
            Paragraph::new("Searching LRCLIB for this track…").style(Style::default().fg(ACCENT)),
            body,
        );
        return;
    }

    let Some(lyrics) = &app.vm.lyrics else {
        let message = match &app.vm.lyrics_error {
            Some(error) => format!(
                "{error}\n\nPress P to paste lyrics, E to open $EDITOR, or F to search again."
            ),
            None => "No lyrics loaded yet.\n\nPress F to search LRCLIB, P to paste, or E to edit."
                .to_string(),
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(DIM))
                .wrap(Wrap { trim: false }),
            body,
        );
        return;
    };

    if let Some(synced) = &lyrics.synced {
        let lines = parse_lrc(synced);
        if !lines.is_empty() {
            let height = body.height as usize;
            let current = current_line(&lines, app.vm.transport.position_ms, lyrics.offset_ms);
            let selected = app.lyrics_selection.filter(|index| *index < lines.len());
            let focus = selected.or(current).unwrap_or(0);
            let start = focus
                .saturating_sub(height / 2)
                .min(lines.len().saturating_sub(height.max(1)));
            let mut rendered = Vec::new();
            for (index, line) in lines.iter().enumerate().skip(start).take(height) {
                let active = current == Some(index);
                let chosen = selected == Some(index);
                let time_style = if chosen || active {
                    Style::default().fg(ACCENT)
                } else {
                    Style::default().fg(DIM)
                };
                let text_style = if chosen {
                    Style::default()
                        .fg(Color::Black)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else if active {
                    Style::default().fg(NOW).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Reset)
                };
                rendered.push(Line::from(vec![
                    Span::styled(
                        format!("{:>8}  ", format_duration(line.timestamp_ms)),
                        time_style,
                    ),
                    Span::styled(line.text.clone(), text_style),
                ]));
            }
            frame.render_widget(Paragraph::new(rendered), body);
            return;
        }
    }

    let scroll = app
        .lyrics_scroll
        .min(lyrics.plain.lines().count().saturating_sub(1)) as u16;
    frame.render_widget(
        Paragraph::new(lyrics.plain.clone())
            .style(Style::default().fg(Color::Reset))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        body,
    );
}

fn draw_stats(frame: &mut Frame, app: &App, block: Block<'_>, area: Rect) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let stats = &app.vm.stats;
    let [cards, gap1, week_title, days, gap2, tops] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(7),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(inner);
    let [all, today, week] = Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(cards);
    stat_card(frame, all, "all time", &human_time(stats.total_ms));
    stat_card(frame, today, "today", &human_time(stats.today_ms));
    stat_card(frame, week, "last 7 days", &human_time(stats.week_ms));

    frame.render_widget(
        Paragraph::new("Last 7 days")
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        week_title,
    );
    let _ = gap1;

    if stats.total_ms == 0 {
        let rest = Rect {
            y: days.y,
            height: inner.bottom().saturating_sub(days.y),
            ..inner
        };
        frame.render_widget(
            Paragraph::new("Play a track and melos will start counting listening time.")
                .style(Style::default().fg(DIM)),
            rest,
        );
        return;
    }

    let max = stats.days.iter().map(|d| d.ms).max().unwrap_or(1).max(1);
    let bar_width = inner.width.saturating_sub(16).max(8) as usize;
    let mut lines = Vec::new();
    for day in &stats.days {
        let filled = ((day.ms as f64 / max as f64) * bar_width as f64).round() as usize;
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<6}", day_label(&day.day)),
                Style::default().fg(DIM),
            ),
            Span::styled(
                format!("{:<18}", "█".repeat(filled)),
                Style::default().fg(if filled > 0 { NOW } else { DIM }),
            ),
            Span::styled(human_time(day.ms), Style::default().fg(DIM)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), days);
    let _ = gap2;

    let [artists, tracks] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(tops);
    frame.render_widget(
        Paragraph::new(name_lines(
            &stats.top_artists,
            artists.width.saturating_sub(4) as usize,
        ))
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(DIM))
                .title(Span::styled(" Top artists ", Style::default().fg(ACCENT))),
        ),
        artists,
    );
    let track_lines: Vec<Line> = stats
        .top_tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let name = format!("{}. {} · {}", i + 1, t.title, t.artist);
            Line::from(vec![
                Span::styled(
                    fit(&name, tracks.width.saturating_sub(10) as usize),
                    Style::default(),
                ),
                Span::styled(format!("  {}", human_time(t.ms)), Style::default().fg(DIM)),
            ])
        })
        .collect();
    let track_lines = if track_lines.is_empty() {
        vec![Line::from(Span::styled(
            "No plays yet.",
            Style::default().fg(DIM),
        ))]
    } else {
        track_lines
    };
    frame.render_widget(
        Paragraph::new(track_lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(DIM))
                .title(Span::styled(" Top tracks ", Style::default().fg(ACCENT))),
        ),
        tracks,
    );
}

fn stat_card(frame: &mut Frame, area: Rect, label: &str, value: &str) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(format!(" {label} "), Style::default().fg(DIM)));
    frame.render_widget(
        Paragraph::new(value.to_string())
            .alignment(Alignment::Center)
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
            .block(block),
        area,
    );
}

fn name_lines(rows: &[melos_proto::NameStat], width: usize) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return vec![Line::from(Span::styled(
            "No plays yet.",
            Style::default().fg(DIM),
        ))];
    }
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let name = format!("{}. {}", i + 1, row.name);
            Line::from(vec![
                Span::styled(fit(&name, width.saturating_sub(10)), Style::default()),
                Span::styled(
                    format!("  {}", human_time(row.ms)),
                    Style::default().fg(DIM),
                ),
            ])
        })
        .collect()
}

fn day_label(day: &str) -> String {
    if day.len() >= 10 {
        day[5..10].to_string()
    } else {
        day.to_string()
    }
}

fn human_time(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn draw_now_playing(frame: &mut Frame, app: &App, area: Rect) {
    let area = area.inner(Margin {
        horizontal: 2,
        vertical: 0,
    });
    let [title_area, artist_area, gauge_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let t = &app.vm.transport;

    let (title, artist, right) = match &t.track {
        Some(track) => {
            let icon = if t.playing { "▶" } else { "❚❚" };
            (
                format!("{icon} {}", track.title),
                if track.artist.is_empty() {
                    String::new()
                } else {
                    track.artist.clone()
                },
                format!(
                    "{} / {}",
                    format_duration(t.position_ms),
                    format_duration(t.duration_ms.max(track.duration_ms)),
                ),
            )
        }
        None => (
            "♪ nothing playing".to_string(),
            "press i to search YouTube, Enter on a track to start".to_string(),
            String::new(),
        ),
    };
    let width = title_area.width as usize;
    let right = fit(&right, width.saturating_sub(12).max(1));
    let right_width = right.width();
    let title = fit(&title, width.saturating_sub(right_width + 1).max(8));
    let line = Line::from(vec![
        Span::styled(title.clone(), Style::default().fg(NOW)),
        Span::raw(" ".repeat(width.saturating_sub(title.width() + right_width))),
        Span::styled(right, Style::default().fg(DIM)),
    ]);
    frame.render_widget(Paragraph::new(line), title_area);
    frame.render_widget(
        Paragraph::new(fit(&artist, width)).style(Style::default().fg(DIM)),
        artist_area,
    );

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

    let hints = format!(
        "j/k move  Enter play  Space pause  n/p next/prev  a queue  A playlist  / filter  i download  ? help   vol {}%{}",
        t.volume,
        if t.muted { " (muted)" } else { "" }
    );
    frame.render_widget(
        Paragraph::new(fit(&hints, hint_area.width as usize)).style(Style::default().fg(DIM)),
        hint_area,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let area = area.inner(Margin {
        horizontal: 2,
        vertical: 0,
    });
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

fn draw_input(frame: &mut Frame, app: &App, kind: &InputKind, buffer: &str, area: Rect) {
    let (label, hint) = match kind {
        InputKind::Filter => ("filter", "Enter apply · Esc clear"),
        InputKind::NewPlaylist => ("new playlist", "Enter create · Esc cancel"),
        InputKind::Download => ("search / url", "Enter search or download · Esc cancel"),
        InputKind::RenameTrack(_) => ("rename track", "Enter save · Esc cancel"),
        InputKind::RenamePlaylist(_) => ("rename playlist", "Enter save · Esc cancel"),
        InputKind::LyricsPaste(_) => ("lyrics", "Ctrl+S save · Esc cancel"),
        InputKind::SourcePath => (
            "music source path",
            "Tab complete · Enter choose import · Esc cancel",
        ),
    };
    let text = format!(" {label}: {buffer}▏  {hint}");
    frame.render_widget(
        Paragraph::new(truncate(&text, area.width as usize))
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
        area,
    );
    let _ = app;
}

fn draw_lyrics_paste(frame: &mut Frame, app: &App, track_id: i64, buffer: &str, area: Rect) {
    let width = area.width.saturating_sub(8).clamp(40, 100);
    let height = area.height.saturating_sub(4).clamp(14, 34);
    let popup = centered_abs(width, height, area);
    frame.render_widget(Clear, popup);
    let name = app
        .vm
        .transport
        .track
        .as_ref()
        .filter(|track| track.id == track_id)
        .map(|track| format!("{} — {}", track.title, track.artist))
        .unwrap_or_else(|| "Paste lyrics".into());
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Paste lyrics · Ctrl+S save · Esc cancel ".to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [title_area, text_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(fit(&name, title_area.width as usize)).style(Style::default().fg(DIM)),
        title_area,
    );
    let text = if buffer.is_empty() {
        "Paste text or LRC here…".to_string()
    } else {
        buffer.to_string()
    };
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(if buffer.is_empty() { DIM } else { Color::Reset }))
            .wrap(Wrap { trim: false }),
        text_area,
    );
}

fn draw_sources(frame: &mut Frame, app: &App, area: Rect, index: usize) {
    let width = area.width.saturating_sub(8).clamp(52, 96);
    let height = ((app.vm.sources.len() as u16).saturating_add(6))
        .clamp(8, 18)
        .min(area.height);
    let popup = centered_abs(width, height, area);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Music sources ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 2 {
        return;
    }
    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    if app.vm.sources.is_empty() {
        frame.render_widget(
            Paragraph::new("No music sources yet. Press a to add a folder or file.")
                .style(Style::default().fg(DIM)),
            list_area,
        );
    } else {
        let items: Vec<ListItem> = app
            .vm
            .sources
            .iter()
            .map(|source| {
                let active = if source.active { "active" } else { "forgotten" };
                let scanned = if source.last_scan.is_some() {
                    "scanned"
                } else {
                    "not scanned"
                };
                let style = if source.active {
                    Style::default()
                } else {
                    Style::default().fg(DIM)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:<8} ", source.kind.label()),
                        Style::default().fg(ACCENT),
                    ),
                    Span::styled(fit(&source.path, width.saturating_sub(36) as usize), style),
                    Span::styled(
                        format!("  {} · {} tracks · {}", active, source.tracks, scanned),
                        Style::default().fg(DIM),
                    ),
                ]))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(index));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::default().bg(SURFACE).add_modifier(Modifier::BOLD)),
            list_area,
            &mut state,
        );
    }
    frame.render_widget(
        Paragraph::new("j/k select · a add · d forget · Enter rescan · R rescan all · Esc close")
            .style(Style::default().fg(DIM)),
        hint_area,
    );
}

fn draw_import_choice(frame: &mut Frame, area: Rect, path: &str, copy: bool) {
    let popup = centered_abs(78, 11, area);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Add music source ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let option = |selected: bool, label: &str, detail: &str| {
        let marker = if selected { "▸ " } else { "  " };
        let style = if selected {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Line::from(vec![
            Span::styled(format!("{marker}{label:<30}"), style),
            Span::styled(detail.to_string(), Style::default().fg(DIM)),
        ])
    };
    let lines = vec![
        Line::from(Span::styled(
            fit(path, inner.width as usize),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        option(
            !copy,
            "Keep files where they are",
            "index in place, no copies",
        ),
        option(copy, "Copy into melos library", "~/Music/melos/Imported"),
        Line::from(""),
        Line::from(Span::styled(
            "k keep · c copy · j/k choose · Enter import · Esc cancel",
            Style::default().fg(DIM),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_delete(frame: &mut Frame, area: Rect, title: &str, choice: DeleteChoice) {
    let popup = centered_abs(68, 10, area);
    frame.render_widget(Clear, popup);
    let option = |selected: bool, label: &str, detail: &str, destructive: bool| {
        let marker = if selected { "▸ " } else { "  " };
        let color = if destructive { DANGER } else { Color::Reset };
        let style = if selected {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };
        Line::from(vec![
            Span::styled(format!("{marker}{label:<26}"), style),
            Span::styled(detail.to_string(), Style::default().fg(DIM)),
        ])
    };
    let lines = vec![
        Line::from(Span::styled(
            truncate(title, 58),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        option(
            choice == DeleteChoice::Melos,
            "Remove from melos only",
            "keep the audio file",
            false,
        ),
        option(
            choice == DeleteChoice::File,
            "Delete the audio file",
            "permanent, cannot be undone",
            true,
        ),
        Line::from(""),
        Line::from(Span::styled(
            "j/k choose · Enter confirm · Esc cancel",
            Style::default().fg(DIM),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().border_type(BorderType::Rounded).title(
            Span::styled(
                " Remove track ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        )),
        popup,
    );
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.keys.help_rows();
    let half = rows.len().div_ceil(2);
    let width = area.width.saturating_sub(6).clamp(40, 82);
    let height = (half as u16 + 4).min(area.height);
    let popup = centered_abs(width, height, area);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
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

fn art_height_for_aspect(aspect: f32, width: u16, font: FontSize, max: u16) -> u16 {
    if width == 0 || font.height == 0 {
        return 0;
    }
    let aspect = if aspect.is_finite() {
        aspect.clamp(0.1, 4.0)
    } else {
        9.0 / 16.0
    };
    let rows = (width as f32 * aspect * (font.width as f32 / font.height as f32)).round() as u16;
    rows.clamp(4, max.max(4))
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
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Add to playlist "),
        )
        .highlight_style(Style::default().bg(SURFACE).add_modifier(Modifier::BOLD))
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
        "pane_lyrics" => "lyrics pane",
        "pane_stats" => "statistics pane",
        "sources" => "music sources",
        "rescan" => "rescan sources",
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
        "visual" => "visual select",
        "move_down" => "move block down",
        "move_up" => "move block up",
        "new_playlist" => "new playlist",
        "remove" => "remove / cancel",
        "delete_track" => "delete track",
        "rename" => "rename",
        "lyrics_paste" => "paste lyrics",
        "lyrics_edit" => "edit lyrics",
        "lyrics_fetch" => "fetch lyrics",
        "lyrics_offset_back" => "lyrics -0.5s",
        "lyrics_offset_fwd" => "lyrics +0.5s",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyMap;
    use melos_proto::{TrackRow, ViewModel};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn artwork_height_follows_the_image_aspect() {
        let font = FontSize::new(10, 20);
        assert_eq!(art_height_for_aspect(9.0 / 16.0, 24, font, 12), 7);
        assert_eq!(art_height_for_aspect(1.0, 24, font, 12), 12);
        assert_eq!(art_height_for_aspect(1.0, 24, font, 6), 6);
        assert_eq!(art_height_for_aspect(9.0 / 16.0, 0, font, 12), 0);
        assert_eq!(art_height_for_aspect(f32::NAN, 24, font, 12), 7);
    }

    #[test]
    fn lyrics_view_renders_synced_lines_in_place() {
        let vm = ViewModel {
            status: "ready".into(),
            transport: melos_proto::Transport {
                track: Some(TrackRow {
                    id: 1,
                    title: "Song".into(),
                    artist: "Artist".into(),
                    album: None,
                    duration_ms: 180_000,
                    missing: false,
                }),
                position_ms: 6_000,
                duration_ms: 180_000,
                playing: true,
                ..Default::default()
            },
            lyrics: Some(melos_proto::LyricsView {
                track_id: 1,
                plain: "first line\nsecond line".into(),
                synced: Some("[00:05.00]first line\n[00:10.00]second line".into()),
                source: "manual".into(),
                offset_ms: 0,
                updated_at: 0,
            }),
            ..Default::default()
        };
        let mut app = App::new(
            vm,
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        );
        app.view = View::Lyrics;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        assert!(app.lyrics_page > 0);
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("first line"));
        assert!(text.contains("second line"));
        assert!(text.contains("manual"));
    }

    #[test]
    fn visual_selection_is_highlighted_and_marked_in_the_title() {
        let vm = ViewModel {
            tracks: vec![
                TrackRow {
                    id: 1,
                    title: "One".into(),
                    artist: "Artist".into(),
                    album: None,
                    duration_ms: 60_000,
                    missing: false,
                },
                TrackRow {
                    id: 2,
                    title: "Two".into(),
                    artist: "Artist".into(),
                    album: None,
                    duration_ms: 60_000,
                    missing: false,
                },
            ],
            status: "ready".into(),
            ..Default::default()
        };
        let mut app = App::new(
            vm,
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        );
        app.main_idx = 1;
        app.visual_anchor = Some(0);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("VISUAL"));
        assert!(buffer.content().iter().any(|cell| cell.bg == VISUAL));
    }

    #[test]
    fn bottom_lines_are_padded_away_from_the_edge() {
        let vm = ViewModel {
            status: "ready".into(),
            transport: melos_proto::Transport {
                track: Some(TrackRow {
                    id: 1,
                    title: "Song".into(),
                    artist: "Artist".into(),
                    album: None,
                    duration_ms: 180_000,
                    missing: false,
                }),
                duration_ms: 180_000,
                playing: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut app = App::new(
            vm,
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 19)].symbol(), "▶");
        assert_eq!(buffer[(2, 23)].symbol(), "r");
    }

    #[test]
    fn music_sources_overlay_lists_paths_and_hints() {
        let vm = ViewModel {
            sources: vec![melos_proto::SourceRow {
                id: 1,
                path: "/music/Albums".into(),
                kind: melos_proto::SourceKind::Folder,
                active: true,
                tracks: 12,
                last_scan: Some(1),
            }],
            ..Default::default()
        };
        let mut app = App::new(
            vm,
            KeyMap::from_config(&melos_proto::config::Config::default().keys),
        );
        app.overlay = Some(Overlay::Sources { index: 0 });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Music sources"));
        assert!(text.contains("/music/Albums"));
        assert!(text.contains("folder"));
        assert!(text.contains("Enter rescan"));
    }
}
