//! Embedded cover-art extraction and cache management.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture};
use lofty::probe::Probe;

/// Extract the first embedded picture to the melos cache.
///
/// Returns `None` when the file has no cover or cannot be read; artwork is
/// optional and must never interfere with playback.
pub fn extract(track_id: i64, source: &Path, mtime: u64) -> Option<String> {
    extract_to(track_id, source, mtime, &melos_proto::paths::art_dir())
        .ok()
        .flatten()
        .map(|path| path.to_string_lossy().into_owned())
}

fn extract_to(track_id: i64, source: &Path, mtime: u64, cache: &Path) -> Result<Option<PathBuf>> {
    let tagged = Probe::open(source)
        .with_context(|| format!("opening {}", source.display()))?
        .read()
        .with_context(|| format!("reading tags from {}", source.display()))?;
    let picture = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .and_then(|tag| tag.pictures().first());
    let Some(picture) = picture else {
        return Ok(None);
    };
    save_picture(cache, track_id, mtime, picture).map(Some)
}

fn save_picture(cache: &Path, track_id: i64, mtime: u64, picture: &Picture) -> Result<PathBuf> {
    let ext = extension(picture.mime_type());
    let path = cache.join(format!("{track_id}-{mtime}.{ext}"));
    if path.is_file() {
        return Ok(path);
    }
    fs::create_dir_all(cache).with_context(|| format!("creating art cache {}", cache.display()))?;
    let tmp = cache.join(format!("{track_id}-{mtime}.{ext}.tmp"));
    fs::write(&tmp, picture.data()).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming {}", path.display()))?;
    Ok(path)
}

/// Drop every cached cover for a track. Called when the track leaves the
/// library so the cache does not grow without bound.
pub fn remove(track_id: i64) {
    remove_from(&melos_proto::paths::art_dir(), track_id);
}

fn remove_from(cache: &Path, track_id: i64) {
    let prefix = format!("{track_id}-");
    let Ok(entries) = fs::read_dir(cache) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(prefix.as_str())
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn extension(mime: Option<&MimeType>) -> &'static str {
    match mime.and_then(MimeType::ext) {
        Some("png") => "png",
        Some("jpg") => "jpg",
        Some("gif") => "gif",
        Some("bmp") => "bmp",
        Some("tif") => "tif",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::picture::{MimeType, Picture};

    #[test]
    fn saves_picture_under_a_stable_cache_name() {
        let dir = std::env::temp_dir().join(format!("melos-art-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let picture = Picture::unchecked(vec![0xFF, 0xD8, 0xFF, 0xD9])
            .mime_type(MimeType::Jpeg)
            .build();
        let path = save_picture(&dir, 42, 1_700_000_000, &picture).unwrap();
        assert_eq!(
            path.file_name().unwrap(),
            "42-1700000000.jpg",
            "cache name combines track id and mtime"
        );
        assert_eq!(std::fs::read(&path).unwrap(), vec![0xFF, 0xD8, 0xFF, 0xD9]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_mime_falls_back_to_bin_extension() {
        assert_eq!(extension(None), "bin");
        assert_eq!(extension(Some(&MimeType::Png)), "png");
    }

    #[test]
    fn removing_a_track_clears_only_its_cache_files() {
        let dir = std::env::temp_dir().join(format!("melos-art-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["7-1.jpg", "7-2.png", "70-1.jpg"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        remove_from(&dir, 7);
        assert!(!dir.join("7-1.jpg").exists());
        assert!(!dir.join("7-2.png").exists());
        assert!(dir.join("70-1.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
