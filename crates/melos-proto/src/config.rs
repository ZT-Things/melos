//! Configuration file (`~/.config/melos/config.toml`).
//!
//! The daemon uses the download/playback settings, the client uses `keys`.
//! Both read the same file, so a remap takes effect for newly started clients.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub music_dir: String,
    pub audio_format: String,
    pub concurrent_downloads: usize,
    pub download: DownloadConfig,
    pub mpv: MpvConfig,
    /// Action name -> key chords, e.g. `next = ["n"]`.
    pub keys: BTreeMap<String, Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            music_dir: "~/Music/melos".into(),
            audio_format: "opus".into(),
            concurrent_downloads: 3,
            download: DownloadConfig::default(),
            mpv: MpvConfig::default(),
            keys: default_keys(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct DownloadConfig {
    /// e.g. "firefox" — passed to yt-dlp as `--cookies-from-browser`.
    pub cookies_from_browser: String,
    pub proxy: String,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MpvConfig {
    pub extra_args: Vec<String>,
}

impl Config {
    /// Load the config. A missing default config is not an error; a missing
    /// explicitly requested config is.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => paths::config_file(),
        };

        if !path.exists() {
            if explicit.is_some() {
                anyhow::bail!("config file not found: {}", path.display());
            }
            return Ok(Self::default());
        }

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.normalize();
        Ok(cfg)
    }

    pub fn normalize(&mut self) {
        if self.audio_format != "opus" && self.audio_format != "m4a" {
            self.audio_format = "opus".into();
        }
        self.concurrent_downloads = self.concurrent_downloads.clamp(1, 5);
        if self.music_dir.trim().is_empty() {
            self.music_dir = Config::default().music_dir;
        }
        for (action, chords) in default_keys() {
            self.keys.entry(action).or_insert(chords);
        }
    }

    pub fn music_dir_path(&self) -> PathBuf {
        paths::expand_tilde(&self.music_dir)
    }
}

pub fn default_keys() -> BTreeMap<String, Vec<String>> {
    let table: &[(&str, &[&str])] = &[
        ("quit", &["q", "ctrl+c"]),
        ("shutdown", &["Q"]),
        ("down", &["j", "down"]),
        ("up", &["k", "up"]),
        ("top", &["g", "home"]),
        ("bottom", &["G", "end"]),
        ("half_down", &["ctrl+d"]),
        ("half_up", &["ctrl+u"]),
        ("page_down", &["pagedown"]),
        ("page_up", &["pageup"]),
        ("confirm", &["enter"]),
        ("pause", &["space"]),
        ("next", &["n"]),
        ("prev", &["p"]),
        ("seek_back", &["left"]),
        ("seek_fwd", &["right"]),
        ("seek_back_long", &["shift+left"]),
        ("seek_fwd_long", &["shift+right"]),
        ("volume_down", &["-"]),
        ("volume_up", &["+"]),
        ("mute", &["m"]),
        ("shuffle", &["s"]),
        ("repeat", &["r"]),
        ("queue_add", &["a"]),
        ("playlist_add", &["A"]),
        ("visual", &["V"]),
        ("move_down", &["J"]),
        ("move_up", &["K"]),
        ("new_playlist", &["N"]),
        ("remove", &["d"]),
        ("delete_track", &["delete"]),
        ("rename", &["e", "F2"]),
        ("filter", &["/"]),
        ("help", &["?"]),
        ("tab", &["tab"]),
        ("pane_library", &["1"]),
        ("pane_playlists", &["2"]),
        ("pane_queue", &["3"]),
        ("pane_downloads", &["4"]),
        ("pane_lyrics", &["5"]),
        ("pane_stats", &["6"]),
        ("lyrics_paste", &["P"]),
        ("lyrics_edit", &["E"]),
        ("lyrics_fetch", &["F"]),
        ("lyrics_offset_back", &["["]),
        ("lyrics_offset_fwd", &["]"]),
        ("download", &["i"]),
        ("playlist_delete", &["x"]),
        ("cancel", &["esc"]),
    ];
    table
        .iter()
        .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let mut cfg = Config {
            concurrent_downloads: 99,
            audio_format: "flac".into(),
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.concurrent_downloads, 5);
        assert_eq!(cfg.audio_format, "opus");
        assert!(cfg.keys.contains_key("quit"));
        assert!(cfg.music_dir_path().is_absolute());
    }

    #[test]
    fn parses_partial_toml() {
        let cfg: Config = toml::from_str(
            r#"
            music_dir = "/srv/music"
            concurrent_downloads = 2
            [download]
            cookies_from_browser = "firefox"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.music_dir, "/srv/music");
        assert_eq!(cfg.concurrent_downloads, 2);
        assert_eq!(cfg.download.cookies_from_browser, "firefox");
        assert_eq!(cfg.keys["next"], vec!["n".to_string()]);
    }
}
