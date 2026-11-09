//! Choosing the main video file and sidecars from a download's output.

use std::path::{Path, PathBuf};

use crate::scan::disc;
use crate::scan::is_video_file;

/// What was found in a download's output.
#[derive(Debug, Default)]
pub struct Selection {
    pub main: Option<(PathBuf, u64)>,
    pub subtitles: Vec<PathBuf>,
    /// Archive or par2 leftovers, which mean extraction did not finish.
    pub leftovers: Vec<PathBuf>,
    /// DVD or Blu-ray folder rips, which are not walked into.
    pub disc_folders: Vec<PathBuf>,
    /// The largest non-sample `.iso`, used only when there is no video.
    /// Its signature is checked by the importer.
    pub image: Option<(PathBuf, u64)>,
}

/// Subtitle sidecar extensions that are carried over.
pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt", "sub", "idx"];

fn ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// `.rar`, `.r00`-style volumes, `.par2` and `.7z`/`.zip` archives.
fn is_leftover(path: &Path) -> bool {
    let e = ext(path);
    matches!(e.as_str(), "rar" | "par2" | "7z" | "zip")
        || (e.len() == 3 && e.starts_with('r') && e[1..].bytes().all(|b| b.is_ascii_digit()))
}

/// Whether a file name marks a sample clip.
fn looks_like_sample(path: &Path) -> bool {
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let in_sample_dir = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case("sample") || n.eq_ignore_ascii_case("samples"))
        .unwrap_or(false);
    in_sample_dir
        || name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|token| token == "sample")
}

/// Walk `output` (a folder or a single file) and pick the largest video that
/// is not a sample and is at least `min_bytes`. Symlinks are not followed.
pub fn select(output: &Path, min_bytes: u64) -> std::io::Result<Selection> {
    let mut selection = Selection::default();
    let mut videos: Vec<(PathBuf, u64)> = Vec::new();
    let mut images: Vec<(PathBuf, u64)> = Vec::new();
    let mut it = walkdir::WalkDir::new(output)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = it.next() {
        let entry = entry.map_err(std::io::Error::other)?;
        let path = entry.path();
        if entry.file_type().is_dir() {
            if disc::disc_folder_format(path)?.is_some() {
                selection.disc_folders.push(path.to_owned());
                it.skip_current_dir();
            } else if entry.depth() > 0
                && disc::is_disc_structure_dir(&entry.file_name().to_string_lossy())
            {
                // The download itself is a bare VIDEO_TS/BDMV tree.
                selection.disc_folders.push(path.to_owned());
                it.skip_current_dir();
            }
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if disc::is_iso_path(path) && !looks_like_sample(path) {
            let size = entry.metadata().map_err(std::io::Error::other)?.len();
            images.push((path.to_owned(), size));
        } else if is_leftover(path) {
            selection.leftovers.push(path.to_owned());
        } else if SUBTITLE_EXTENSIONS.contains(&ext(path).as_str()) {
            selection.subtitles.push(path.to_owned());
        } else if is_video_file(path) && !looks_like_sample(path) {
            let size = entry.metadata().map_err(std::io::Error::other)?.len();
            videos.push((path.to_owned(), size));
        }
    }
    selection.main = videos
        .into_iter()
        .filter(|(_, size)| *size >= min_bytes)
        .max_by_key(|(_, size)| *size);
    selection.image = images.into_iter().max_by_key(|(_, size)| *size);
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_folders_are_units_not_videos() {
        let tmp = tempfile::tempdir().unwrap();
        let rel = tmp.path().join("Movie.2010.BluRay");
        std::fs::create_dir_all(rel.join("BDMV/STREAM")).unwrap();
        std::fs::write(rel.join("BDMV/index.bdmv"), b"x").unwrap();
        std::fs::write(rel.join("BDMV/STREAM/00001.m2ts"), vec![0u8; 4096]).unwrap();
        let sel = select(tmp.path(), 1).unwrap();
        assert!(sel.main.is_none());
        assert_eq!(sel.disc_folders, vec![rel]);
    }

    #[test]
    fn classifies_names() {
        assert!(is_leftover(Path::new("a.rar")));
        assert!(is_leftover(Path::new("a.r07")));
        assert!(is_leftover(Path::new("a.vol00+01.PAR2")));
        assert!(!is_leftover(Path::new("a.rmvb")));
        assert!(looks_like_sample(Path::new("x/movie-sample.mkv")));
        assert!(looks_like_sample(Path::new("x/Sample/movie.mkv")));
        assert!(!looks_like_sample(Path::new("x/Samples of Life.mkv")));
    }
}
