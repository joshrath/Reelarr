//! Library scanner tests. A fake prober is used except in the ffprobe test,
//! which runs only when `ffmpeg` and `ffprobe` are installed.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reelarr_media::scan::{
    FfprobeProber, InMemoryMediaFileStore, MediaFileStore, ProbeError, Prober, ScanError,
    ScanOptions, Scanner, parse_ffprobe_json, run_periodic,
};
use reelarr_types::{
    LibraryRoot, LibraryRootId, LibraryRootKind, MediaFileStatus, MediaProbe, StreamKind,
};

#[derive(Default)]
struct FakeProber {
    calls: AtomicUsize,
}

impl Prober for FakeProber {
    async fn probe(&self, path: &Path) -> Result<MediaProbe, ProbeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if path.to_string_lossy().contains("corrupt") {
            return Err(ProbeError::Failed("invalid data found".into()));
        }
        Ok(MediaProbe {
            container: vec!["matroska".into()],
            duration_ms: Some(1000),
            ..MediaProbe::default()
        })
    }
}

fn root(dir: &Path) -> LibraryRoot {
    LibraryRoot {
        id: LibraryRootId(1),
        path: dir.to_string_lossy().into_owned(),
        kind: LibraryRootKind::Library,
        rescan_interval_secs: Some(3600),
    }
}

fn write(dir: &Path, rel: &str, bytes: &[u8]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn scanner() -> Scanner<FakeProber, InMemoryMediaFileStore> {
    Scanner::new(
        FakeProber::default(),
        InMemoryMediaFileStore::new(),
        ScanOptions::default(),
    )
}

#[tokio::test]
async fn first_scan_adds_video_files_only() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "Movie A (2001)/Movie A (2001).mkv", b"aaaa");
    write(dir.path(), "Movie A (2001)/Movie A (2001).nfo", b"x");
    write(dir.path(), "Movie A (2001)/Movie A (2001).en.srt", b"x");
    write(dir.path(), "Nested/Deeper/Movie B.MP4", b"bb");
    write(dir.path(), ".trash/Old.mkv", b"x");
    write(dir.path(), "Movie C.mkv.reelarr-partial", b"x");

    let s = scanner();
    let report = s.scan_root(&root(dir.path())).await.unwrap();
    assert_eq!(report.added, 2);
    assert!(report.probe_failures.is_empty());

    let files = s.store().list_by_root(LibraryRootId(1)).await.unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.status == MediaFileStatus::Present));
    assert!(files.iter().all(|f| f.probe.is_some()));
    let a = files
        .iter()
        .find(|f| f.path.ends_with("(2001).mkv"))
        .unwrap();
    assert_eq!(a.size_bytes, 4);
}

#[tokio::test]
async fn rescan_only_probes_changed_files() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "one.mkv", b"1");
    write(dir.path(), "two.mkv", b"2");
    let s = scanner();
    let r = root(dir.path());
    s.scan_root(&r).await.unwrap();
    assert_eq!(s_calls(&s), 2);

    let report = s.scan_root(&r).await.unwrap();
    assert_eq!(report.unchanged, 2);
    assert_eq!(s_calls(&s), 2, "unchanged files are not re-probed");

    write(dir.path(), "two.mkv", b"changed size");
    let report = s.scan_root(&r).await.unwrap();
    assert_eq!((report.updated, report.unchanged), (1, 1));
    assert_eq!(s_calls(&s), 3);
}

fn s_calls(s: &Scanner<FakeProber, InMemoryMediaFileStore>) -> usize {
    s.prober().calls.load(Ordering::SeqCst)
}

#[tokio::test]
async fn missing_files_are_marked_not_deleted_and_can_return() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "keep.mkv", b"k");
    write(dir.path(), "gone.mkv", b"g");
    let s = scanner();
    let r = root(dir.path());
    s.scan_root(&r).await.unwrap();

    let gone_bytes = std::fs::read(dir.path().join("gone.mkv")).unwrap();
    let gone_mtime = std::fs::metadata(dir.path().join("gone.mkv"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::remove_file(dir.path().join("gone.mkv")).unwrap();
    let report = s.scan_root(&r).await.unwrap();
    assert_eq!(report.marked_missing, 1);
    let files = s.store().list_by_root(r.id).await.unwrap();
    assert_eq!(files.len(), 2, "missing file is kept");
    let gone = files.iter().find(|f| f.path.ends_with("gone.mkv")).unwrap();
    assert!(matches!(gone.status, MediaFileStatus::Missing { .. }));

    // Second scan does not re-mark it.
    assert_eq!(s.scan_root(&r).await.unwrap().marked_missing, 0);

    // Put it back unchanged: restored without probing.
    std::fs::write(dir.path().join("gone.mkv"), gone_bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(dir.path().join("gone.mkv"))
        .unwrap()
        .set_modified(gone_mtime)
        .unwrap();
    let before = s_calls(&s);
    let report = s.scan_root(&r).await.unwrap();
    assert_eq!(report.restored, 1);
    assert_eq!(s_calls(&s), before);
}

#[tokio::test]
async fn unavailable_or_unmounted_root_marks_nothing_missing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "share/movie.mkv", b"m");
    let share = dir.path().join("share");
    let s = scanner();
    let r = root(&share);
    s.scan_root(&r).await.unwrap();

    // Share mount point still exists but is empty (unmounted).
    std::fs::remove_file(share.join("movie.mkv")).unwrap();
    assert!(matches!(
        s.scan_root(&r).await,
        Err(ScanError::RootLooksUnmounted { known: 1, .. })
    ));

    // Mount point gone entirely.
    std::fs::remove_dir(&share).unwrap();
    assert!(matches!(
        s.scan_root(&r).await,
        Err(ScanError::RootUnavailable { .. })
    ));

    let files = s.store().list_by_root(r.id).await.unwrap();
    assert_eq!(files[0].status, MediaFileStatus::Present);
}

#[tokio::test]
async fn probe_failures_are_reported_stored_and_retried() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "corrupt.mkv", b"x");
    let s = scanner();
    let r = root(dir.path());
    let report = s.scan_root(&r).await.unwrap();
    assert_eq!(report.added, 1);
    assert_eq!(report.probe_failures.len(), 1);
    let files = s.store().list_by_root(r.id).await.unwrap();
    assert!(files[0].probe.is_none());

    let report = s.scan_root(&r).await.unwrap();
    assert_eq!(report.updated, 1, "unprobed file is retried");
    assert_eq!(s_calls(&s), 2);
}

#[tokio::test]
async fn manual_import_root_is_scanned_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "dropped/Some.Movie.2010.mkv", b"x");
    let s = scanner();
    let mut r = root(dir.path());
    r.id = LibraryRootId(7);
    r.kind = LibraryRootKind::ManualImport;
    assert_eq!(s.scan_root(&r).await.unwrap().added, 1);
    assert_eq!(
        s.store()
            .list_by_root(LibraryRootId(7))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn periodic_rescan_runs_on_interval_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.mkv", b"a");
    let s = scanner();
    let r = root(dir.path()); // 3600 s interval
    let (tx, rx) = tokio::sync::watch::channel(false);

    let run = run_periodic(&s, &r, rx);
    let drive = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        write(dir.path(), "b.mkv", b"b");
        tokio::time::sleep(Duration::from_secs(3600)).await;
        tx.send(true).unwrap();
    };
    tokio::join!(run, drive);
    let files = s.store().list_by_root(r.id).await.unwrap();
    assert_eq!(files.len(), 2, "second scan picked up the new file");
}

#[test]
fn parses_real_ffprobe_output() {
    let probe = parse_ffprobe_json(include_bytes!("fixtures/ffprobe_h264_aac.json")).unwrap();
    assert!(probe.container.iter().any(|c| c == "mp4"));
    assert_eq!(probe.duration_ms, Some(1000));
    assert!(probe.bit_rate.unwrap() > 0);
    let video = &probe.streams[0];
    assert_eq!(video.kind, StreamKind::Video);
    assert_eq!(video.codec.as_deref(), Some("h264"));
    assert_eq!((video.width, video.height), (Some(320), Some(240)));
    assert_eq!(video.language, None, "und is dropped");
    let audio = &probe.streams[1];
    assert_eq!(audio.kind, StreamKind::Audio);
    assert_eq!(audio.codec.as_deref(), Some("aac"));
    assert_eq!(audio.channels, Some(2));
    assert_eq!(audio.language.as_deref(), Some("eng"));
}

fn have(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn ffprobe_prober_reads_a_generated_clip() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("clip.mkv");
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=size=160x120:rate=10:duration=1")
        .args(["-c:v", "mpeg4"])
        .arg(&clip)
        .status()
        .unwrap();
    assert!(status.success());

    let prober = FfprobeProber::default();
    let probe = prober.probe(&clip).await.unwrap();
    assert!(probe.container.iter().any(|c| c == "matroska"));
    assert_eq!(probe.streams[0].codec.as_deref(), Some("mpeg4"));

    std::fs::write(dir.path().join("bad.mkv"), b"not a video").unwrap();
    assert!(matches!(
        prober.probe(&dir.path().join("bad.mkv")).await,
        Err(ProbeError::Failed(_))
    ));

    let missing = FfprobeProber {
        binary: "/nonexistent/ffprobe".into(),
        ..FfprobeProber::default()
    };
    assert!(matches!(
        missing.probe(&clip).await,
        Err(ProbeError::Spawn { .. })
    ));
}
