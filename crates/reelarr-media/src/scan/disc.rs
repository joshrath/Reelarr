//! Recognizing disc rips: `.iso` images and DVD (`VIDEO_TS`) or Blu-ray
//! (`BDMV`) folders.
//!
//! Detection only reads directory listings and, for `.iso` files, a few
//! volume descriptor sectors. Nothing is mounted, extracted or written.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};

/// ISO 9660 and UDF volume descriptors start at sector 16 of 2048 bytes.
const SECTOR: u64 = 2048;
const FIRST_DESCRIPTOR: u64 = 16;
/// How many descriptor sectors to look at. Real images put the primary
/// descriptor or the UDF recognition sequence within the first few.
const DESCRIPTORS_TO_READ: u64 = 16;

/// Folder names that belong to a disc structure. The scanner never treats
/// files inside them as separate videos.
pub const DISC_STRUCTURE_DIRS: &[&str] = &["video_ts", "bdmv", "audio_ts", "certificate", "aacs"];

/// Whether `path` has an `.iso` extension.
pub fn is_iso_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
}

/// Whether `name` is a disc-structure folder name (case-insensitive).
pub fn is_disc_structure_dir(name: &str) -> bool {
    DISC_STRUCTURE_DIRS.contains(&name.to_ascii_lowercase().as_str())
}

/// Whether `file` lives under a disc-structure directory of `root`
/// (`BDMV`, `VIDEO_TS`, and the other names in [`DISC_STRUCTURE_DIRS`]).
/// Sibling extras such as `root/feature.mkv` or `root/Extras/foo.mkv`
/// are not part of the disc payload.
pub fn is_disc_payload_path(root: &Path, file: &Path) -> bool {
    file.strip_prefix(root)
        .map(|rel| {
            rel.components()
                .any(|c| c.as_os_str().to_str().is_some_and(is_disc_structure_dir))
        })
        .unwrap_or(false)
}

/// Reads the volume descriptor area of an image and reports whether it has
/// an ISO 9660 (`CD001`) or UDF (`BEA01`/`NSR02`/`NSR03`) signature.
pub fn has_disc_signature(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(FIRST_DESCRIPTOR * SECTOR))?;
    let mut buf = vec![0u8; (DESCRIPTORS_TO_READ * SECTOR) as usize];
    let mut read = 0;
    while read < buf.len() {
        match file.read(&mut buf[read..])? {
            0 => break,
            n => read += n,
        }
    }
    Ok(buf[..read].chunks(SECTOR as usize).any(|sector| {
        sector.len() >= 6 && matches!(&sector[1..6], b"CD001" | b"BEA01" | b"NSR02" | b"NSR03")
    }))
}

/// Which disc format a folder holds, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscFormat {
    Dvd,
    BluRay,
}

/// Whether `dir` is the top of a disc rip: it contains a `VIDEO_TS` folder
/// with `VIDEO_TS.IFO` or a `.VOB`, or a `BDMV` folder with `index.bdmv` or
/// a `STREAM` folder. Names are matched case-insensitively.
pub fn disc_folder_format(dir: &Path) -> io::Result<Option<DiscFormat>> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let wanted: &dyn Fn(&str, bool) -> bool = match name.as_str() {
            "video_ts" => &|n, is_dir| !is_dir && (n == "video_ts.ifo" || n.ends_with(".vob")),
            "bdmv" => &|n, is_dir| (!is_dir && n == "index.bdmv") || (is_dir && n == "stream"),
            _ => continue,
        };
        for inner in std::fs::read_dir(entry.path())? {
            let inner = inner?;
            let inner_name = inner.file_name().to_string_lossy().to_ascii_lowercase();
            if wanted(&inner_name, inner.file_type()?.is_dir()) {
                return Ok(Some(if name == "video_ts" {
                    DiscFormat::Dvd
                } else {
                    DiscFormat::BluRay
                }));
            }
        }
    }
    Ok(None)
}

/// Limits for summing a disc folder's size.
#[derive(Debug, Clone, Copy)]
pub struct FolderLimits {
    pub max_entries: usize,
    pub deadline: Instant,
}

/// Total size and newest modification time (Unix seconds) of the files in a
/// disc folder. Stops with an error once `limits` are exceeded, so a slow or
/// failing share does not hold up the scan; the caller keeps the existing
/// entry in that case.
pub fn folder_size(dir: &Path, limits: FolderLimits) -> Result<(u64, i64), String> {
    let mut total = 0u64;
    let mut newest = 0i64;
    for (count, entry) in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if count >= limits.max_entries {
            return Err(format!(
                "{} has more than {} entries; skipped",
                dir.display(),
                limits.max_entries
            ));
        }
        if Instant::now() > limits.deadline {
            return Err(format!("reading {} took too long; skipped", dir.display()));
        }
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().is_file() {
            continue;
        }
        if !super::is_scannable_name(entry.path()) {
            continue;
        }
        if !is_disc_payload_path(dir, entry.path()) {
            continue;
        }
        let meta = entry.metadata().map_err(|e| e.to_string())?;
        total = total.saturating_add(meta.len());
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        newest = newest.max(modified);
    }
    Ok((total, newest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        let iso = tmp.path().join("a.iso");
        let mut data = vec![0u8; 18 * 2048];
        data[16 * 2048] = 1;
        data[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
        std::fs::write(&iso, &data).unwrap();
        assert!(has_disc_signature(&iso).unwrap());

        let udf = tmp.path().join("b.iso");
        let mut data = vec![0u8; 20 * 2048];
        data[17 * 2048 + 1..17 * 2048 + 6].copy_from_slice(b"NSR03");
        std::fs::write(&udf, &data).unwrap();
        assert!(has_disc_signature(&udf).unwrap());

        let junk = tmp.path().join("c.iso");
        std::fs::write(&junk, b"not a disc").unwrap();
        assert!(!has_disc_signature(&junk).unwrap());
        assert!(is_iso_path(Path::new("x/Movie.ISO")));
        assert!(!is_iso_path(Path::new("x/movie.mkv")));
    }
}
