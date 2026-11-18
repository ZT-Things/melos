//! Configurable key chords mapped to named actions.

use std::collections::{BTreeMap, HashMap};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub struct KeyMap {
    map: HashMap<String, String>,
    by_action: BTreeMap<String, Vec<String>>,
}

impl KeyMap {
    pub fn from_config(keys: &BTreeMap<String, Vec<String>>) -> Self {
        let mut map = HashMap::new();
        let mut by_action = BTreeMap::new();
        for (action, chords) in keys {
            for chord in chords {
                let normalized = normalize(chord);
                if normalized.is_empty() {
                    continue;
                }
                map.insert(normalized.clone(), action.clone());
                by_action
                    .entry(action.clone())
                    .or_insert_with(Vec::new)
                    .push(normalized);
            }
        }
        Self { map, by_action }
    }

    pub fn action(&self, key: KeyEvent) -> Option<&str> {
        let chord = chord_of(key);
        if chord.is_empty() {
            return None;
        }
        self.map.get(&chord).map(String::as_str)
    }

    pub fn chords(&self, action: &str) -> Vec<String> {
        let mut chords = self.by_action.get(action).cloned().unwrap_or_default();
        chords.sort();
        chords
    }

    /// Rows for the help overlay: (action, "key1, key2").
    pub fn help_rows(&self) -> Vec<(String, String)> {
        const ORDER: &[&str] = &[
            "down",
            "up",
            "top",
            "bottom",
            "half_down",
            "half_up",
            "page_down",
            "page_up",
            "tab",
            "pane_library",
            "pane_playlists",
            "pane_queue",
            "pane_downloads",
            "pane_lyrics",
            "pane_stats",
            "sources",
            "rescan",
            "confirm",
            "pause",
            "next",
            "prev",
            "seek_back",
            "seek_fwd",
            "seek_back_long",
            "seek_fwd_long",
            "volume_up",
            "volume_down",
            "mute",
            "shuffle",
            "repeat",
            "queue_add",
            "playlist_add",
            "visual",
            "move_down",
            "move_up",
            "new_playlist",
            "remove",
            "delete_track",
            "rename",
            "lyrics_paste",
            "lyrics_edit",
            "lyrics_fetch",
            "lyrics_offset_back",
            "lyrics_offset_fwd",
            "filter",
            "download",
            "playlist_delete",
            "help",
            "cancel",
            "quit",
            "shutdown",
        ];
        let mut rows = Vec::new();
        for action in ORDER {
            let mut chords = self.chords(action);
            let alias = match *action {
                "lyrics_paste" => Some("p"),
                "lyrics_edit" => Some("e"),
                "lyrics_fetch" => Some("f"),
                _ => None,
            };
            if let Some(alias) = alias
                && !chords.iter().any(|chord| chord == alias)
            {
                chords.push(alias.to_string());
            }
            if chords.is_empty() {
                continue;
            }
            rows.push((action.to_string(), chords.join(", ")));
        }
        rows
    }
}

pub fn chord_of(key: KeyEvent) -> String {
    let m = key.modifiers;
    let mut parts: Vec<String> = Vec::new();
    if m.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".into());
    }
    if m.contains(KeyModifiers::ALT) {
        parts.push("alt".into());
    }
    if m.contains(KeyModifiers::SHIFT) && !matches!(key.code, KeyCode::Char(_)) {
        parts.push("shift".into());
    }
    let base = match key.code {
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) => {
            if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::ALT) {
                c.to_ascii_lowercase().to_string()
            } else {
                c.to_string()
            }
        }
        KeyCode::Enter => "enter".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::F(n) => format!("f{n}"),
        _ => return String::new(),
    };
    parts.push(base);
    parts.join("+")
}

fn normalize(input: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return String::new();
    }
    if input == "+" {
        return "+".into();
    }
    let raw: Vec<&str> = input.split('+').collect();
    let mut parts: Vec<String> = Vec::new();
    for part in &raw[..raw.len().saturating_sub(1)] {
        parts.push(part.trim().to_ascii_lowercase());
    }
    let last = raw.last().copied().unwrap_or("");
    let has_modifier = raw.len() > 1;
    let base = if last.chars().count() == 1 && !has_modifier {
        last.to_string()
    } else {
        last.trim().to_ascii_lowercase()
    };
    parts.push(base);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn maps_defaults() {
        let mut cfg = melos_proto::config::Config::default().keys;
        cfg.insert("prev".into(), vec!["p".into()]);
        let map = KeyMap::from_config(&cfg);
        assert_eq!(
            map.action(key(KeyCode::Char('j'), KeyModifiers::NONE)),
            Some("down")
        );
        assert_eq!(
            map.action(key(KeyCode::Char('Q'), KeyModifiers::SHIFT)),
            Some("shutdown")
        );
        assert_eq!(
            map.action(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some("quit")
        );
        assert_eq!(
            map.action(key(KeyCode::Left, KeyModifiers::SHIFT)),
            Some("seek_back_long")
        );
    }

    #[test]
    fn configurable_ctrl_j_and_k_are_distinct_from_enter() {
        let mut cfg = BTreeMap::new();
        cfg.insert("half_down".to_string(), vec!["ctrl+j".to_string()]);
        cfg.insert("half_up".to_string(), vec!["ctrl+k".to_string()]);
        let map = KeyMap::from_config(&cfg);
        assert_eq!(
            map.action(key(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Some("half_down")
        );
        assert_eq!(
            map.action(key(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            Some("half_up")
        );
    }

    #[test]
    fn space_and_plus() {
        let map = KeyMap::from_config(&melos_proto::config::Config::default().keys);
        assert_eq!(
            map.action(key(KeyCode::Char(' '), KeyModifiers::NONE)),
            Some("pause")
        );
        assert_eq!(
            map.action(key(KeyCode::Char('+'), KeyModifiers::SHIFT)),
            Some("volume_up")
        );
    }

    #[test]
    fn normalization() {
        assert_eq!(normalize("Ctrl+D"), "ctrl+d");
        assert_eq!(normalize("Q"), "Q");
        assert_eq!(normalize("shift+LEFT"), "shift+left");
        assert_eq!(normalize("+"), "+");
    }

    #[test]
    fn help_lists_new_playlist() {
        let map = KeyMap::from_config(&melos_proto::config::Config::default().keys);
        assert!(
            map.help_rows()
                .contains(&("new_playlist".to_string(), "N".to_string()))
        );
    }

    #[test]
    fn help_shows_unshifted_lyrics_aliases() {
        let map = KeyMap::from_config(&melos_proto::config::Config::default().keys);
        let rows = map.help_rows();
        for (action, chord) in [
            ("lyrics_paste", "p"),
            ("lyrics_edit", "e"),
            ("lyrics_fetch", "f"),
        ] {
            assert!(
                rows.iter()
                    .any(|(name, keys)| name == action && keys.split(", ").any(|k| k == chord)),
                "{action} should list {chord}"
            );
        }
    }
}
