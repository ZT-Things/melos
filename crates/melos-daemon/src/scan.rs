//! Local music source scanning and copying.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use melos_proto::{SourceKind, SourceRow};

use crate::db::NewTrack;

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "m4a", "aac", "wav", "wv", "ape", "aiff", "aif",
];

#[derive(Debug, Default)]
pub struct ScanResult {
    pub source_id: i64,
    pub tracks: Vec<NewTrack>,
    pub seen: Vec<String>,
    pub errors: Vec<String>,
}

pub fn scan_source(source: &SourceRow, known: &HashMap<String, i64>) -> ScanResult {
    let mut result = ScanResult {
        source_id: source.id,
        ..ScanResult::default()
    };
    let root = Path::new(&source.path);
    match source.kind {
        SourceKind::File => scan_file(root, source.id, known, &mut result),
        SourceKind::Folder | SourceKind::Managed => scan_dir(root, source.id, known, &mut result),
    }
    result
}

fn scan_dir(dir: &Path, source_id: i64, known: &HashMap<String, i64>, result: &mut ScanResult) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            result.errors.push(format!("{}: {error}", dir.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                result.errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            scan_dir(&path, source_id, known, result);
        } else if file_type.is_file() && is_audio(&path) {
            scan_file(&path, source_id, known, result);
        }
    }
}

fn scan_file(path: &Path, source_id: i64, known: &HashMap<String, i64>, result: &mut ScanResult) {
    if !is_audio(path) {
        return;
    }
    let path_string = path.display().to_string();
    result.seen.push(path_string.clone());
    let mtime = match modified(path) {
        Ok(mtime) => mtime,
        Err(error) => {
            result.errors.push(format!("{path_string}: {error}"));
            return;
        }
    };
    if known.get(&path_string) == Some(&mtime) {
        return;
    }
    result
        .tracks
        .push(read_track(path, source_id, path_string, mtime));
}

fn read_track(path: &Path, source_id: i64, path_string: String, mtime: i64) -> NewTrack {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".into());
    let (mut title, mut artist) = (stem.clone(), "Unknown artist".to_string());
    if let Some((left, right)) = stem.split_once(" - ")
        && !left.trim().is_empty()
        && !right.trim().is_empty()
    {
        artist = left.trim().to_string();
        title = right.trim().to_string();
    }
    let mut album = None;
    let mut duration_ms = 0;
    if let Ok(tagged) = lofty::read_from_path(path) {
        duration_ms = tagged.properties().duration().as_millis() as u64;
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(value) = tag.title().filter(|s| !s.trim().is_empty()) {
                title = value.to_string();
            }
            if let Some(value) = tag.artist().filter(|s| !s.trim().is_empty()) {
                artist = value.to_string();
            }
            album = tag.album().map(|a| a.to_string());
        }
    }
    NewTrack {
        yt_id: format!("local:{path_string}"),
        path: path_string,
        title,
        artist,
        album,
        duration_ms,
        mtime,
        source_id: Some(source_id),
    }
}

fn modified(path: &Path) -> Result<i64> {
    Ok(fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .modified()
        .with_context(|| format!("mtime of {}", path.display()))?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
}

pub(crate) fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Copy a file or folder into `managed/Imported/`, returning the destination.
/// Existing destinations are never overwritten.
pub fn import_target(source: &Path, managed: &Path) -> Result<PathBuf> {
    let source = source
        .canonicalize()
        .with_context(|| format!("resolving {}", source.display()))?;
    let managed = managed
        .canonicalize()
        .with_context(|| format!("resolving {}", managed.display()))?;
    if source.starts_with(&managed) {
        bail!("that path is already inside the melos library");
    }
    let name = source
        .file_name()
        .with_context(|| format!("{} has no file name", source.display()))?;
    Ok(managed.join("Imported").join(name))
}

pub fn copy_into_managed(source: &Path, managed: &Path) -> Result<PathBuf> {
    let source = source
        .canonicalize()
        .with_context(|| format!("resolving {}", source.display()))?;
    let managed = managed
        .canonicalize()
        .with_context(|| format!("resolving {}", managed.display()))?;
    let target = import_target(&source, &managed)?;
    let imported = target
        .parent()
        .with_context(|| format!("{} has no parent", target.display()))?;
    fs::create_dir_all(imported).with_context(|| format!("creating {}", imported.display()))?;
    if target.exists() {
        bail!("already exists: {}", target.display());
    }
    let name = target
        .file_name()
        .with_context(|| format!("{} has no file name", target.display()))?;
    let staging = imported.join(format!(
        ".melos-import-{}-{}",
        std::process::id(),
        name.to_string_lossy()
    ));
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_file(&staging);
    let copied = if source.is_dir() {
        copy_dir(&source, &staging)
    } else {
        fs::copy(&source, &staging)
            .map(|_| ())
            .with_context(|| format!("copying {}", source.display()))
    };
    if let Err(error) = copied {
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    fs::rename(&staging, &target)
        .with_context(|| format!("moving import into {}", target.display()))?;
    Ok(target)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&path, &target)?;
        } else if file_type.is_file() {
            fs::copy(&path, &target).with_context(|| format!("copying {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use melos_proto::SourceKind;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "melos-scan-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn source(path: &Path, kind: SourceKind) -> SourceRow {
        SourceRow {
            id: 7,
            path: path.display().to_string(),
            kind,
            active: true,
            tracks: 0,
            last_scan: None,
        }
    }

    #[test]
    fn scans_audio_files_and_falls_back_to_artist_title_names() {
        let root = temp("titles");
        fs::write(root.join("Artist - Song.opus"), b"not audio").unwrap();
        fs::write(root.join("notes.txt"), b"ignore").unwrap();
        let result = scan_source(&source(&root, SourceKind::Folder), &HashMap::new());
        assert_eq!(result.tracks.len(), 1);
        assert_eq!(result.tracks[0].title, "Song");
        assert_eq!(result.tracks[0].artist, "Artist");
        assert_eq!(result.tracks[0].source_id, Some(7));
        assert!(result.tracks[0].yt_id.starts_with("local:"));
        assert_eq!(result.seen.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_files_are_seen_but_not_reread() {
        let root = temp("mtime");
        let file = root.join("a.flac");
        fs::write(&file, b"x").unwrap();
        let mtime = modified(&file).unwrap();
        let known = HashMap::from([(file.display().to_string(), mtime)]);
        let result = scan_source(&source(&root, SourceKind::Folder), &known);
        assert!(result.tracks.is_empty());
        assert_eq!(result.seen.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_symlinks_are_not_followed() {
        let root = temp("symlink");
        let other = temp("symlink-target");
        fs::write(other.join("hidden.opus"), b"x").unwrap();
        std::os::unix::fs::symlink(&other, root.join("link")).unwrap();
        let result = scan_source(&source(&root, SourceKind::Folder), &HashMap::new());
        assert!(result.tracks.is_empty());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn copying_a_folder_preserves_structure_and_refuses_collisions() {
        let root = temp("copy-src");
        let managed = temp("copy-managed");
        fs::create_dir_all(root.join("Album")).unwrap();
        fs::write(root.join("Album/song.opus"), b"x").unwrap();
        let target = copy_into_managed(&root, &managed).unwrap();
        assert!(target.join("Album/song.opus").is_file());
        assert!(root.join("Album/song.opus").is_file());
        let error = copy_into_managed(&root, &managed).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(managed).unwrap();
    }

    #[test]
    fn copying_a_single_file_uses_the_imported_folder() {
        let root = temp("copy-file-src");
        let managed = temp("copy-file-managed");
        let file = root.join("song.opus");
        fs::write(&file, b"x").unwrap();
        let target = copy_into_managed(&file, &managed).unwrap();
        assert_eq!(target, managed.join("Imported/song.opus"));
        assert!(target.is_file());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(managed).unwrap();
    }
}
