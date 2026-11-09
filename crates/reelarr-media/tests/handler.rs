//! The core `ImportHandler` adapter: root choice, repeat calls and crash
//! re-entry, on a real SQLite file and real folders.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use reelarr_core::{
    CompletedDownload, DownloadSource, FailureCode, ImportHandler, ImportResult, JobId,
};
use reelarr_db::Database;
use reelarr_media::db::{
    LedgerEntry, LedgerState, SqliteImportLedger, SqliteLibraryRootStore, SqliteMediaFileStore,
    SqliteMovieStore,
};
use reelarr_media::import::{HandlerOptions, MediaImportHandler, TitleYearMatcher, TransferMode};
use reelarr_media::scan::{MediaFileStore, NewMediaFile, ProbeError, Prober};
use reelarr_media::store::MovieStore;
use reelarr_types::{
    Artwork, ExternalIds, LibraryRoot, LibraryRootKind, MediaFileId, MediaKind, MediaProbe,
    MediaStream, MovieId, MovieMetadata, NewLibraryRoot, StreamKind,
};

struct FakeProber;

impl Prober for FakeProber {
    async fn probe(&self, _path: &Path) -> Result<MediaProbe, ProbeError> {
        Ok(MediaProbe {
            container: vec!["matroska".into()],
            duration_ms: Some(1000),
            bit_rate: None,
            streams: vec![MediaStream {
                index: 0,
                kind: StreamKind::Video,
                codec: Some("h264".into()),
                profile: None,
                width: Some(1920),
                height: Some(1080),
                channels: None,
                language: None,
                default: true,
                forced: false,
            }],
        })
    }
}

type Handler = MediaImportHandler<FakeProber, TitleYearMatcher<SqliteMovieStore>>;

const RELEASE: &str = "The.Example.Movie.2004.1080p.BluRay.x264-GRP";

struct Env {
    tmp: PathBuf,
    db: Database,
    downloads: PathBuf,
    movie: MovieId,
}

async fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap().keep();
    let db = Database::connect(tmp.join("reelarr.db")).await.unwrap();
    let downloads = tmp.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let movie = SqliteMovieStore::new(&db)
        .insert(
            MovieMetadata {
                external_ids: ExternalIds {
                    tmdb_id: 1001,
                    imdb_id: None,
                },
                title: "The Example Movie".into(),
                original_title: None,
                year: Some(2004),
                overview: None,
                runtime_minutes: None,
                artwork: Artwork::default(),
            },
            0,
        )
        .await
        .unwrap()
        .id;
    Env {
        tmp,
        db,
        downloads,
        movie,
    }
}

impl Env {
    async fn root(&self, name: &str) -> LibraryRoot {
        let path = self.tmp.join(name);
        std::fs::create_dir_all(&path).unwrap();
        SqliteLibraryRootStore::new(&self.db)
            .add(NewLibraryRoot {
                path: path.to_string_lossy().into_owned(),
                kind: LibraryRootKind::Library,
                rescan_interval_secs: None,
            })
            .await
            .unwrap()
    }

    fn handler(&self, tweak: impl FnOnce(&mut HandlerOptions)) -> Handler {
        let mut options = HandlerOptions {
            min_video_bytes: 10,
            ..HandlerOptions::default()
        };
        tweak(&mut options);
        MediaImportHandler::new(
            &self.db,
            FakeProber,
            TitleYearMatcher::new(SqliteMovieStore::new(&self.db)),
            options,
        )
    }

    /// A download folder holding one video, returned as the video path.
    fn download(&self, bytes: &[u8]) -> PathBuf {
        let video = self.downloads.join(RELEASE).join(format!("{RELEASE}.mkv"));
        std::fs::create_dir_all(video.parent().unwrap()).unwrap();
        std::fs::write(&video, bytes).unwrap();
        video
    }

    fn completed(&self, job_id: JobId) -> CompletedDownload {
        CompletedDownload {
            job_id,
            movie_id: Some(self.movie),
            output_path: self.downloads.join(RELEASE),
            category: Some("movies".into()),
            source: DownloadSource::BuiltinNzb,
            release_title: RELEASE.into(),
        }
    }

    async fn file_path(&self, id: MediaFileId) -> PathBuf {
        SqliteMediaFileStore::new(&self.db)
            .get(id)
            .await
            .unwrap()
            .unwrap()
            .path
            .into()
    }

    async fn file_count(&self) -> usize {
        SqliteMediaFileStore::new(&self.db)
            .list_by_movie(self.movie)
            .await
            .unwrap()
            .len()
    }
}

fn dest_in(root: &LibraryRoot) -> PathBuf {
    Path::new(&root.path)
        .join("The Example Movie (2004)")
        .join("The Example Movie (2004) [1080p].mkv")
}

fn imported(result: ImportResult) -> MediaFileId {
    match result {
        ImportResult::Imported { media_file_id } => media_file_id,
        other => panic!("expected Imported, got {other:?}"),
    }
}

fn failed(result: ImportResult) -> (bool, Option<FailureCode>) {
    match result {
        ImportResult::Failed {
            retryable, code, ..
        } => (retryable, code),
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// A pending ledger row as a crashed first attempt would have left it.
async fn crashed_plan(
    env: &Env,
    job: JobId,
    root: &LibraryRoot,
    source: &Path,
    mode: TransferMode,
) {
    SqliteImportLedger::new(&env.db)
        .put_pending(&LedgerEntry {
            job_id: job.to_string(),
            state: LedgerState::Pending,
            movie_id: env.movie,
            root_id: root.id,
            kind: MediaKind::File,
            source_path: source.to_string_lossy().into_owned(),
            dest_path: dest_in(root).to_string_lossy().into_owned(),
            mode,
            size_bytes: std::fs::metadata(source).unwrap().len(),
            media_file_id: None,
        })
        .await
        .unwrap();
    std::fs::create_dir_all(dest_in(root).parent().unwrap()).unwrap();
}

#[tokio::test]
async fn imports_into_the_only_root_and_repeats_return_the_same_file() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    let handler = env.handler(|_| {});
    let job = JobId::new();

    let id = imported(handler.import(env.completed(job)).await);
    let dest = env.file_path(id).await;
    assert_eq!(dest, dest_in(&root));
    assert_eq!(
        std::fs::metadata(&dest).unwrap().ino(),
        std::fs::metadata(&source).unwrap().ino()
    );

    assert_eq!(imported(handler.import(env.completed(job)).await), id);
    assert_eq!(env.file_count().await, 1);
    let entry = SqliteImportLedger::new(&env.db)
        .get(&job.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.state, LedgerState::Done);
    assert_eq!(entry.media_file_id, Some(id));
}

#[tokio::test]
async fn crash_after_hardlink_finishes_the_record() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    let job = JobId::new();
    crashed_plan(&env, job, &root, &source, TransferMode::HardlinkOrCopy).await;
    std::fs::hard_link(&source, dest_in(&root)).unwrap();

    let id = imported(env.handler(|_| {}).import(env.completed(job)).await);
    assert_eq!(env.file_path(id).await, dest_in(&root));
    assert_eq!(env.file_count().await, 1);
}

#[tokio::test]
async fn crash_after_copy_accepts_identical_bytes_and_clears_partials() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    let job = JobId::new();
    crashed_plan(&env, job, &root, &source, TransferMode::Copy).await;
    std::fs::copy(&source, dest_in(&root)).unwrap();
    let partial = PathBuf::from(format!("{}.reelarr-partial", dest_in(&root).display()));
    std::fs::write(&partial, b"main fea").unwrap();

    let handler = env.handler(|o| o.mode = TransferMode::Copy);
    let id = imported(handler.import(env.completed(job)).await);
    assert_eq!(env.file_path(id).await, dest_in(&root));
    assert!(!partial.exists());
    assert!(source.exists());
}

#[tokio::test]
async fn crash_mid_copy_transfers_again() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    let job = JobId::new();
    crashed_plan(&env, job, &root, &source, TransferMode::Copy).await;
    let partial = PathBuf::from(format!("{}.reelarr-partial", dest_in(&root).display()));
    std::fs::write(&partial, b"main fea").unwrap();

    let handler = env.handler(|o| o.mode = TransferMode::Copy);
    let id = imported(handler.import(env.completed(job)).await);
    assert_eq!(
        std::fs::read(env.file_path(id).await).unwrap(),
        b"main feature bytes"
    );
    assert!(!partial.exists());
}

#[tokio::test]
async fn crash_after_move_finishes_without_the_source() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    let job = JobId::new();
    crashed_plan(&env, job, &root, &source, TransferMode::Move).await;
    std::fs::rename(&source, dest_in(&root)).unwrap();

    let handler = env.handler(|o| o.mode = TransferMode::Move);
    let id = imported(handler.import(env.completed(job)).await);
    assert_eq!(env.file_path(id).await, dest_in(&root));
    assert_eq!(imported(handler.import(env.completed(job)).await), id);
}

#[tokio::test]
async fn crash_after_cross_filesystem_move_copy_drops_the_source() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    let job = JobId::new();
    crashed_plan(&env, job, &root, &source, TransferMode::Move).await;
    std::fs::copy(&source, dest_in(&root)).unwrap();

    let handler = env.handler(|o| o.mode = TransferMode::Move);
    imported(handler.import(env.completed(job)).await);
    assert!(!source.exists());
}

#[tokio::test]
async fn someone_elses_file_at_the_destination_is_a_terminal_conflict() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    std::fs::create_dir_all(dest_in(&root).parent().unwrap()).unwrap();
    std::fs::write(dest_in(&root), b"a different movie!").unwrap();

    let result = env
        .handler(|_| {})
        .import(env.completed(JobId::new()))
        .await;
    assert_eq!(
        failed(result),
        (false, Some(FailureCode::DestinationConflict))
    );
    assert_eq!(
        std::fs::read(dest_in(&root)).unwrap(),
        b"a different movie!"
    );
    assert!(source.exists());
}

#[tokio::test]
async fn offline_root_is_retryable_and_never_falls_back() {
    let env = env().await;
    let root = env.root("library").await;
    let other = env.root("other").await;
    env.download(b"main feature bytes");
    std::fs::remove_dir(&root.path).unwrap();

    let handler = env.handler(|o| o.default_root = Some(root.id));
    let result = handler.import(env.completed(JobId::new())).await;
    assert_eq!(failed(result), (true, Some(FailureCode::RootOffline)));
    assert!(!dest_in(&other).exists());
    assert_eq!(env.file_count().await, 0);
}

#[tokio::test]
async fn several_roots_need_a_default() {
    let env = env().await;
    env.root("a").await;
    let b = env.root("b").await;
    env.download(b"main feature bytes");

    let result = env
        .handler(|_| {})
        .import(env.completed(JobId::new()))
        .await;
    assert_eq!(failed(result), (false, Some(FailureCode::NoDefaultRoot)));

    let handler = env.handler(|o| o.default_root = Some(b.id));
    let id = imported(handler.import(env.completed(JobId::new())).await);
    assert_eq!(env.file_path(id).await, dest_in(&b));
}

#[tokio::test]
async fn a_movie_stays_on_the_root_it_already_lives_on() {
    let env = env().await;
    let a = env.root("a").await;
    let b = env.root("b").await;
    let existing = Path::new(&b.path).join("old.mkv");
    std::fs::write(&existing, b"old copy").unwrap();
    SqliteMediaFileStore::new(&env.db)
        .insert(NewMediaFile {
            movie_id: Some(env.movie),
            root_id: b.id,
            path: existing.to_string_lossy().into_owned(),
            size_bytes: 8,
            modified_at: 0,
            probe: None,
            kind: MediaKind::File,
        })
        .await
        .unwrap();
    env.download(b"main feature bytes");

    let handler = env.handler(|o| o.default_root = Some(a.id));
    let id = imported(handler.import(env.completed(JobId::new())).await);
    assert_eq!(env.file_path(id).await, dest_in(&b));
}

#[tokio::test]
async fn no_library_root_is_terminal_and_unknown_title_is_no_match() {
    let env = env().await;
    env.download(b"main feature bytes");
    let handler = env.handler(|_| {});
    let result = handler.import(env.completed(JobId::new())).await;
    assert_eq!(failed(result), (false, Some(FailureCode::NoDefaultRoot)));

    env.root("library").await;
    let mut download = env.completed(JobId::new());
    download.movie_id = None;
    download.release_title = "Some.Other.Film.1999.720p".into();
    assert_eq!(handler.import(download).await, ImportResult::NoMatch);
}

#[tokio::test]
async fn crash_after_move_then_offline_root_is_retryable() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    let job = JobId::new();
    crashed_plan(&env, job, &root, &source, TransferMode::Move).await;
    std::fs::rename(&source, dest_in(&root)).unwrap();
    std::fs::remove_dir_all(&root.path).unwrap();

    let handler = env.handler(|o| o.mode = TransferMode::Move);
    let result = handler.import(env.completed(job)).await;
    assert_eq!(failed(result), (true, Some(FailureCode::RootOffline)));
    let entry = SqliteImportLedger::new(&env.db)
        .get(&job.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.state, LedgerState::Pending);
    assert!(entry.media_file_id.is_none());
}

#[tokio::test]
async fn crash_after_move_clears_a_stale_partial() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    let job = JobId::new();
    crashed_plan(&env, job, &root, &source, TransferMode::Move).await;
    std::fs::rename(&source, dest_in(&root)).unwrap();
    let partial = PathBuf::from(format!("{}.reelarr-partial", dest_in(&root).display()));
    std::fs::write(&partial, b"leftover").unwrap();

    let handler = env.handler(|o| o.mode = TransferMode::Move);
    imported(handler.import(env.completed(job)).await);
    assert!(!partial.exists());
}

#[tokio::test]
async fn a_symlink_inside_the_root_cannot_point_the_import_outside() {
    let env = env().await;
    let root = env.root("library").await;
    let outside = env.tmp.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, Path::new(&root.path).join("escape")).unwrap();
    env.download(b"main feature bytes");

    let handler = env.handler(|o| {
        o.template = "escape/{Title} ({Year}) [{Quality}]".into();
    });
    let result = handler.import(env.completed(JobId::new())).await;
    let (retryable, _) = failed(result);
    assert!(!retryable);
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
}

#[tokio::test]
async fn a_reused_ledger_destination_outside_the_root_is_rejected() {
    let env = env().await;
    let root = env.root("library").await;
    let source = env.download(b"main feature bytes");
    let job = JobId::new();
    let stolen = env.tmp.join("stolen.mkv");
    SqliteImportLedger::new(&env.db)
        .put_pending(&LedgerEntry {
            job_id: job.to_string(),
            state: LedgerState::Pending,
            movie_id: env.movie,
            root_id: root.id,
            kind: MediaKind::File,
            source_path: source.to_string_lossy().into_owned(),
            dest_path: stolen.to_string_lossy().into_owned(),
            mode: TransferMode::HardlinkOrCopy,
            size_bytes: std::fs::metadata(&source).unwrap().len(),
            media_file_id: None,
        })
        .await
        .unwrap();

    let result = env.handler(|_| {}).import(env.completed(job)).await;
    let (retryable, _) = failed(result);
    assert!(!retryable);
    assert!(!stolen.exists());
}
