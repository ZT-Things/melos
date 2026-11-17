//! XDG paths used by melos, without pulling in a directories crate.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(name: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(name) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("melos")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("melos")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("melos")
}

pub fn db_path() -> PathBuf {
    data_dir().join("library.db")
}

pub fn state_file() -> PathBuf {
    state_dir().join("state.json")
}

pub fn daemon_log() -> PathBuf {
    state_dir().join("daemon.log")
}

/// Runtime directory: `$XDG_RUNTIME_DIR/melos`, falling back to `/tmp/melos-<uid>`.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("melos"),
        _ => {
            // SAFETY: getuid is always safe to call.
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/melos-{uid}"))
        }
    }
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("daemon.sock")
}

pub fn mpv_socket_path() -> PathBuf {
    runtime_dir().join("mpv.sock")
}

pub fn default_music_dir() -> PathBuf {
    home().join("Music/melos")
}

/// Expand a leading `~` or `~/` in a user-supplied path.
pub fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return home();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let h = home();
        assert_eq!(expand_tilde("~"), h);
        assert_eq!(expand_tilde("~/Music"), h.join("Music"));
        assert_eq!(expand_tilde("/tmp/x"), PathBuf::from("/tmp/x"));
    }
}
