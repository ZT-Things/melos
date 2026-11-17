//! Queue, shuffle, repeat and the currently loaded track.

use anyhow::Result;
use melos_proto::Repeat;

use crate::db::Db;
use crate::mpv::MpvHandle;

pub struct Player {
    pub queue: Vec<i64>,
    pub pos: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub volume: i64,
    pub muted: bool,
    pub playing: bool,
    order: Vec<usize>,
    mpv: Option<MpvHandle>,
    loaded: bool,
}

impl Player {
    pub fn new(mpv: Option<MpvHandle>, volume: i64, shuffle: bool, repeat: Repeat) -> Self {
        Self {
            queue: Vec::new(),
            pos: None,
            shuffle,
            repeat,
            position_ms: 0,
            duration_ms: 0,
            volume: volume.clamp(0, 100),
            muted: false,
            playing: false,
            order: Vec::new(),
            mpv,
            loaded: false,
        }
    }

    pub fn mpv_mut(&mut self) -> Option<&mut MpvHandle> {
        self.mpv.as_mut()
    }

    pub fn has_mpv(&self) -> bool {
        self.mpv.is_some()
    }

    pub fn current_id(&self) -> Option<i64> {
        self.pos.and_then(|i| self.queue.get(i)).copied()
    }

    pub fn is_active(&self) -> bool {
        self.loaded && self.pos.is_some()
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn set_loaded(&mut self, loaded: bool) {
        self.loaded = loaded;
        if !loaded {
            self.playing = false;
        }
    }

    /// Handle an mpv `end-file` reason.
    ///
    /// mpv emits `stop` whenever a file is replaced with `loadfile ...
    /// replace`, so treating every end-file as the end of playback makes the
    /// daemon claim it is paused while mpv keeps playing. Returns the play
    /// state before a real end (`eof` / `error`), or `None` for a replacement.
    pub fn file_ended(&mut self, reason: &str) -> Option<bool> {
        if !matches!(reason, "eof" | "error") {
            return None;
        }
        let was_playing = self.playing;
        self.set_loaded(false);
        Some(was_playing)
    }

    /// Replace the queue with `context`, selecting `track_id`.
    pub fn play_track(&mut self, track_id: i64, context: &[i64], db: &Db) -> Result<()> {
        let queue: Vec<i64> = if context.contains(&track_id) {
            context.to_vec()
        } else {
            vec![track_id]
        };
        let index = queue.iter().position(|id| *id == track_id).unwrap_or(0);
        self.queue = queue;
        self.pos = Some(index);
        self.rebuild_order();
        self.load_current(db)
    }

    pub fn play_queue_index(&mut self, index: usize, db: &Db) -> Result<()> {
        if index >= self.queue.len() {
            anyhow::bail!("queue index out of range");
        }
        self.pos = Some(index);
        self.rebuild_order();
        self.load_current(db)
    }

    fn load_current(&mut self, db: &Db) -> Result<()> {
        let Some(id) = self.current_id() else {
            return Ok(());
        };
        let Some((row, path)) = db.track(id)? else {
            anyhow::bail!("track {id} is no longer in the library");
        };
        if row.missing || !std::path::Path::new(&path).exists() {
            db.set_missing(id, true)?;
            anyhow::bail!("file is missing: {}", row.title);
        }
        db.bump_play_count(id)?;
        self.position_ms = 0;
        self.duration_ms = row.duration_ms;
        self.playing = true;
        self.loaded = true;
        if let Some(mpv) = &self.mpv {
            mpv.load(&path);
            mpv.set_volume(self.volume);
            mpv.set_mute(self.muted);
        }
        Ok(())
    }

    pub fn toggle_pause(&mut self) -> Result<()> {
        if !self.is_active() {
            anyhow::bail!("nothing is playing");
        }
        self.playing = !self.playing;
        if let Some(mpv) = &self.mpv {
            mpv.set_pause(!self.playing);
        }
        Ok(())
    }

    pub fn set_paused(&mut self, paused: bool) {
        // mpv reports `pause=false` while sitting idle; ignore it when no
        // file is loaded so the daemon doesn't claim to be playing silence.
        if self.is_active() {
            self.playing = !paused;
        }
    }

    /// Set the play/pause state explicitly (used by MPRIS).
    pub fn set_playing(&mut self, playing: bool) {
        if !self.is_active() {
            return;
        }
        self.playing = playing;
        if let Some(mpv) = &self.mpv {
            mpv.set_pause(!playing);
        }
    }

    pub fn stop(&mut self) {
        self.playing = false;
        self.loaded = false;
        self.position_ms = 0;
        if let Some(mpv) = &self.mpv {
            mpv.stop();
        }
    }

    pub fn seek_relative(&mut self, seconds: i64) {
        let delta_ms = seconds.saturating_mul(1000);
        let target = (self.position_ms as i64 + delta_ms).max(0) as u64;
        self.position_ms = self.cap_position(target);
        if let Some(mpv) = &self.mpv {
            mpv.seek_relative(seconds as f64);
        }
    }

    pub fn seek_absolute(&mut self, seconds: u64) {
        self.position_ms = self.cap_position(seconds.saturating_mul(1000));
        if let Some(mpv) = &self.mpv {
            mpv.seek_absolute(seconds as f64);
        }
    }

    fn cap_position(&self, ms: u64) -> u64 {
        if self.duration_ms > 0 {
            ms.min(self.duration_ms)
        } else {
            ms
        }
    }

    pub fn set_volume(&mut self, volume: i64) {
        self.volume = volume.clamp(0, 100);
        if let Some(mpv) = &self.mpv {
            mpv.set_volume(self.volume);
        }
    }

    pub fn volume_delta(&mut self, delta: i64) {
        self.set_volume(self.volume + delta);
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(mpv) = &self.mpv {
            mpv.set_mute(self.muted);
        }
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.cycle();
    }

    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.rebuild_order();
    }

    pub fn set_shuffle(&mut self, on: bool) {
        if self.shuffle != on {
            self.shuffle = on;
            self.rebuild_order();
        }
    }

    pub fn set_repeat(&mut self, mode: Repeat) {
        self.repeat = mode;
    }

    /// Advance the queue. `manual` advances even when repeat is off but only to
    /// the end; auto-advance at the end stops playback unless repeat is `All`.
    pub fn next(&mut self, db: &Db, manual: bool) -> Result<()> {
        if self.queue.is_empty() {
            anyhow::bail!("queue is empty");
        }
        if self.repeat == Repeat::One && !manual {
            return self.load_current(db);
        }
        let current = self.pos.unwrap_or(0);
        let Some(order_pos) = self.order.iter().position(|idx| *idx == current) else {
            return self.play_queue_index(0, db);
        };
        if order_pos + 1 < self.order.len() {
            let target = self.order[order_pos + 1];
            self.pos = Some(target);
            self.load_current(db)
        } else if self.repeat == Repeat::All {
            let target = self.order[0];
            self.pos = Some(target);
            self.load_current(db)
        } else {
            self.stop();
            self.position_ms = 0;
            anyhow::bail!("end of queue")
        }
    }

    pub fn prev(&mut self, db: &Db, manual: bool) -> Result<()> {
        if self.queue.is_empty() {
            anyhow::bail!("queue is empty");
        }
        if manual && self.position_ms > 3_000 && self.is_active() {
            self.seek_absolute(0);
            return Ok(());
        }
        let current = self.pos.unwrap_or(0);
        let Some(order_pos) = self.order.iter().position(|idx| *idx == current) else {
            return self.play_queue_index(0, db);
        };
        if order_pos > 0 {
            let target = self.order[order_pos - 1];
            self.pos = Some(target);
            self.load_current(db)
        } else if self.repeat == Repeat::All {
            let target = *self.order.last().unwrap_or(&0);
            self.pos = Some(target);
            self.load_current(db)
        } else {
            self.seek_absolute(0);
            Ok(())
        }
    }

    pub fn on_eof(&mut self, db: &Db) -> Result<()> {
        if self.repeat == Repeat::One {
            return self.load_current(db);
        }
        match self.next(db, false) {
            Ok(()) => Ok(()),
            Err(e) if e.to_string() == "end of queue" => {
                self.playing = false;
                self.position_ms = 0;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn queue_add(&mut self, id: i64) {
        self.queue.push(id);
        self.rebuild_order();
    }

    pub fn queue_remove(&mut self, index: usize) {
        if index >= self.queue.len() {
            return;
        }
        self.queue.remove(index);
        match self.pos {
            Some(p) if p == index => {
                if self.queue.is_empty() {
                    self.pos = None;
                    self.stop();
                } else {
                    self.pos = Some(p.min(self.queue.len() - 1));
                }
            }
            Some(p) if p > index => self.pos = Some(p - 1),
            _ => {}
        }
        self.rebuild_order();
    }

    pub fn queue_clear(&mut self) {
        self.queue.clear();
        self.pos = None;
        self.stop();
    }

    fn rebuild_order(&mut self) {
        let n = self.queue.len();
        if !self.shuffle {
            self.order = (0..n).collect();
            return;
        }
        let mut order: Vec<usize> = (0..n).collect();
        fisher_yates(&mut order);
        if let Some(current) = self.pos
            && let Some(anchor) = order.iter().position(|idx| *idx == current)
        {
            order.rotate_left(anchor);
        }
        self.order = order;
    }

    /// Rebuild the shuffle order but keep the current track at the front.
    pub fn reseed_shuffle(&mut self) {
        self.rebuild_order();
    }
}

/// xorshift seeded from the clock; good enough for a shuffle and avoids a
/// dependency on `rand`.
fn fisher_yates(items: &mut [usize]) {
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffle_order_is_a_permutation() {
        let mut items: Vec<usize> = (0..200).collect();
        fisher_yates(&mut items);
        let mut sorted = items.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..200).collect::<Vec<_>>());
    }

    fn loaded_player() -> Player {
        let mut player = Player::new(None, 70, false, Repeat::Off);
        player.pos = Some(0);
        player.queue = vec![1];
        player.loaded = true;
        player.playing = true;
        player
    }

    #[test]
    fn replacing_a_file_does_not_count_as_the_end_of_playback() {
        let mut player = loaded_player();
        assert_eq!(player.file_ended("stop"), None);
        assert!(player.playing, "audio keeps playing during a file replace");
        assert!(player.is_loaded());
        assert_eq!(player.current_id(), Some(1));
    }

    #[test]
    fn eof_is_a_real_end_and_reports_the_previous_play_state() {
        let mut player = loaded_player();
        assert_eq!(player.file_ended("eof"), Some(true));
        assert!(!player.playing);
        assert!(!player.is_loaded());
    }

    #[test]
    fn a_redirect_does_not_clear_playback() {
        let mut player = loaded_player();
        assert_eq!(player.file_ended("redirect"), None);
        assert!(player.playing);
    }

    #[test]
    fn a_paused_end_reports_paused_and_clears_loaded() {
        let mut player = loaded_player();
        player.set_playing(false);
        assert_eq!(player.file_ended("eof"), Some(false));
        assert!(!player.playing);
        assert!(!player.is_loaded());
    }
}
