# melos

A fast, minimal terminal music player. Search YouTube, download audio, build
playlists, and play everything from a clean keyboard-driven interface.

`melos` is a thin TUI client. A resident daemon (`melosd`) owns the library,
downloads, playback and MPRIS, so opening the interface is instant and music
keeps playing after you close it.

```
┌ melos v0.2.0 ────────┬ Library ──────────────────────────────────────────┐
│ ▄▀▄▀▄▀▄▀▄▀▄▀▄▀▄▀▄▀▄ │   A Short Walk With You       Purrple Cat   1:32  │
│ ▀▄▀▄▀▄▀▄▀▄▀▄▀▄▀▄▀▄▀ │ ▶ Night Drive                 Nils          4:10  │
│ A Short Walk With... │   Rain on Glass               ...           3:58  │
│ Purrple Cat          │                                                   │
│▸ Library        42   │                                                   │
│  Playlists       3   ├───────────────────────────────────────────────────┤
│  Queue           2   │ ▶ Night Drive — Nils                              │
│  Downloads           │ ████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ │
│  Statistics          │ j/k move  Space pause  n/p track  q detach         │
└──────────────────────┴───────────────────────────────────────────────────┘
```

## Features

- **Instant startup** — the daemon stays warm, playback keeps running while the
  UI is closed. `q` detaches, `Q` stops playback and exits.
- **Keyboard first** — vim-style `j`/`k`, plus arrows, `space`, `n`/`p`, `/`,
  and other shortcuts you already know. Every binding is remappable.
- **YouTube downloads** — search from inside the app or paste a URL. Downloads
  run concurrently in the background while you keep browsing.
- **Playlists** — create, rename, add tracks, remove tracks, and play from any
  row. Rename tracks too; your display names never rewrite the audio tags.
- **Album art** — embedded thumbnails appear in the sidebar, with a clean
  placeholder when a track has none.
- **Listening statistics** — all-time, today, last seven days, plus top artists
  and tracks. Time is counted while the daemon plays, even with the UI closed.
- **Deletion with a choice** — remove a track from melos only, or delete the
  downloaded audio file as well.
- **MPRIS** — media keys and desktop widgets work through
  `org.mpris.MediaPlayer2.melos`, even with no UI attached.
- **Small and quiet** — Rust, ratatui, SQLite index, mpv for audio. Idle CPU is
  effectively zero, and the daemon does no work while no client is attached.
- **No account, no cloud** — files live in `~/Music/melos`; the database is only
  an index and can be rebuilt from the files.

## Install

### Arch Linux (AUR)

```sh
yay -S melos          # build from source
# or
yay -S melos-bin      # prebuilt x86_64 binary
```

Optional: start the daemon at login through socket activation. This is not
enabled automatically:

```sh
systemctl --user enable --now melos.socket
```

Without it, `melos` starts `melosd` on demand.

### From source

Requirements: Rust 1.85+ (edition 2024), `mpv`, `yt-dlp`, SQLite development
headers, and `ffmpeg` (used by yt-dlp for extraction and thumbnails).

```sh
git clone https://github.com/ZT-Things/melos
cd melos
cargo build --release
install -Dm755 target/release/melos  ~/.local/bin/melos
install -Dm755 target/release/melosd ~/.local/bin/melosd
```

## Usage

```sh
melos                  # open the player (starts the daemon if needed)
melos status           # is the daemon running, what is playing?
melos stop             # stop playback and shut the daemon down
melos --trace-startup  # print startup timings when the UI exits
```

### Downloading music

Press `i` from any pane. Type a search such as `purrple cat rain` and press
Enter, or paste a YouTube URL and press Enter.

- URL: downloads immediately.
- Search: shows results in the Downloads pane; select one and press Enter.
- Downloads continue while you browse or detach. `d` cancels the selected job.
- Finished tracks are tagged, indexed, and shown in the Library.
- Files are stored as
  `~/Music/melos/<uploader>/<title> [<video-id>].<ext>`.

### Playlists and queue

- `N` creates a playlist, `A` adds the selected track to one, `e`/`F2` renames
  one, `x` deletes a playlist.
- `a` appends the selected track to the queue, `3` opens the queue, `d` removes
  an item.
- `e`/`F2` on a track in the Library or a playlist renames its display name.
- `Enter` in the Library or a playlist starts that track with the whole visible
  list as context, so `n`/`p` move through it.

### Removing tracks

Press `Delete` (or `d` in the Library) on a track. A dialog offers:

- **Remove from melos only** — forget the track and its playlist entries; the
  audio file stays on disk.
- **Delete the audio file** — also permanently delete the downloaded file. This
  cannot be undone.

Safe removal is selected first. File deletion is refused for anything outside
the configured music directory.

### Listening statistics

Press `5` for the Statistics pane: all-time time, today, the last seven days,
and the artists and tracks you have played most. Time is counted while `melosd`
is playing, including when the TUI is detached. Only playback through melos is
counted.

### Keys

| Key | Action |
| --- | --- |
| `j` `k` / `↓` `↑` | move down / up |
| `g` `G` / `Home` `End` | first / last |
| `Ctrl+d` `Ctrl+u` | half page down / up |
| `PgDn` `PgUp` | page down / up |
| `Tab` | switch between sidebar and main pane |
| `1` `2` `3` `4` `5` | Library, Playlists, Queue, Downloads, Statistics |
| `Enter` | play / open / confirm |
| `Space` | pause / resume |
| `n` `p` | next / previous track |
| `←` `→` | seek 5 seconds |
| `Shift+←` `Shift+→` | seek 30 seconds |
| `+` `-` | volume up / down |
| `m` | mute |
| `s` | shuffle |
| `r` | cycle repeat (off / all / one) |
| `a` | add to queue |
| `A` | add to playlist |
| `N` | new playlist |
| `e` / `F2` | rename selected playlist or track |
| `Delete` | remove track from the library (choose whether to delete the file) |
| `d` | remove from queue/playlist, cancel download, or remove a library track |
| `i` | download (search or URL) |
| `/` | filter the current list |
| `?` | help |
| `q` / `Ctrl+c` | detach (keep playing) |
| `Q` | stop playback and shut down the daemon |
| `Esc` | cancel input / close overlay |

The help overlay (`?`) always reflects the active keymap.

## Configuration

Create `~/.config/melos/config.toml`. All fields are optional.

```toml
# Where downloaded music is stored. `~` is expanded.
music_dir = "~/Music/melos"

# "opus" (small, good quality) or "m4a".
audio_format = "opus"

# Parallel downloads, 1–5.
concurrent_downloads = 3

[download]
# Pass cookies for age-restricted or members-only videos.
cookies_from_browser = ""   # e.g. "firefox"
proxy = ""                  # e.g. "socks5://127.0.0.1:1080"
extra_args = []             # extra yt-dlp arguments

[mpv]
extra_args = []             # extra mpv arguments

# Remap any action. Multiple keys per action are allowed.
[keys]
down = ["j", "down"]
up = ["k", "up"]
quit = ["q", "ctrl+c"]
shutdown = ["Q"]
next = ["n"]
prev = ["p"]
pause = ["space"]
queue_add = ["a"]
playlist_add = ["A"]
delete_track = ["delete"]
rename = ["e", "F2"]
pane_stats = ["5"]
download = ["i"]
```

Action names: `quit`, `shutdown`, `down`, `up`, `top`, `bottom`, `half_down`,
`half_up`, `page_down`, `page_up`, `confirm`, `pause`, `next`, `prev`,
`seek_back`, `seek_fwd`, `seek_back_long`, `seek_fwd_long`, `volume_down`,
`volume_up`, `mute`, `shuffle`, `repeat`, `queue_add`, `playlist_add`,
`new_playlist`, `remove`, `delete_track`, `rename`, `filter`, `help`, `tab`,
`pane_library`, `pane_playlists`, `pane_queue`, `pane_downloads`, `pane_stats`,
`download`, `playlist_delete`, `cancel`.

Chords use `ctrl+`, `alt+`, and `shift+` prefixes, e.g. `ctrl+d`,
`shift+left`, `alt+j`. A plain uppercase letter such as `Q` means Shift is
required.

## Files

| Path | Purpose |
| --- | --- |
| `~/Music/melos/` | downloaded audio files |
| `~/.local/share/melos/library.db` | SQLite index (safe to delete) |
| `~/.local/state/melos/state.json` | queue, position, volume, shuffle/repeat |
| `~/.local/state/melos/daemon.log` | daemon output when spawned by the TUI |
| `$XDG_RUNTIME_DIR/melos/daemon.sock` | client/daemon socket |
| `~/.cache/melos/art/` | extracted cover art cache |
| `~/.config/melos/config.toml` | configuration |

The library database stores metadata extracted at download time. Missing files
are marked missing rather than silently removed, so a temporarily unmounted
music directory does not destroy your playlists.

## How it works

```
melos (TUI) ──newline JSON over unix socket──▶ melosd (daemon)
                                                ├─ mpv (JSON IPC)
                                                ├─ yt-dlp + ffmpeg
                                                ├─ SQLite library
                                                └─ MPRIS (D-Bus)
```

The daemon is authoritative; the client just renders snapshots and sends
actions. Snapshot updates are coalesced, playback position is pushed at most
four times per second, and the daemon idles at 0% CPU when no client is
attached.

## v0.2 limitations

- Only tracks downloaded through the app are indexed. Importing an existing
  music folder is not implemented yet.
- Renaming changes melos' display name only; audio tags and filenames are left
  untouched.
- Listening time covers playback through melos only, and is tracked while the
  daemon is running.
- Album art depends on the terminal: Kitty graphics where supported, otherwise
  a colored half-block rendering.
- Queue advancement loads one file at a time; mpv's gapless mode helps, but
  this is not sample-perfect album playback.
- Video is never downloaded; audio only.
- No lyrics, equalizer, scrobbling, or cloud sync. By design.

## Legal

`melos` does not download anything by itself. It invokes `yt-dlp`, which you
must install and use in accordance with YouTube's Terms of Service and the law
where you live. Only download content you have the right to download.

## License

MIT © 2026 ZT
