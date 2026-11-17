//! End to end through `AppState` with the real media importer (#76): the
//! server's own constructor wires `MediaImportHandler` (ffprobe prober,
//! title/year matcher over the SQLite movie store, default handler options)
//! into the download supervisor. A `ReadyForImport` job left over from before
//! a restart is imported on start without any NNTP provider. The movie lands
//! in the library folder and the download's files are cleaned up afterwards.
use std::{os::unix::fs::MetadataExt, path::Path, sync::Arc, time::Duration};

use reelarr_core::{JobState, JobStore};
use reelarr_db::{Database, SqliteRecoveryPaths, secrets::XChaChaSecretCipher};
use reelarr_media::{
    db::{LedgerState, SqliteImportLedger, SqliteMediaFileStore, SqliteMovieStore},
    store::MovieStore,
};
use reelarr_nzb::worker::RecoveryPaths;
use reelarr_server::{
    downloads::NewDownload,
    state::{AppState, DownloadOptions},
};
use reelarr_types::{
    Artwork, ExternalIds, LibraryRootKind, MediaFileStatus, MovieMetadata, NewLibraryRoot,
};

const RELEASE: &str = "The.Example.Movie.2004.1080p.BluRay.x264-GRP";

/// Above the production handler's 50 MiB sample threshold. Sparse, so it
/// costs no disk; the hardlink import never copies it.
const VIDEO_BYTES: u64 = 64 * 1024 * 1024;

fn nzb() -> String {
    format!(
        "<?xml version=\"1.0\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\
         <head><meta type=\"title\">{RELEASE}</meta></head>\
         <file poster=\"tester@example.test\" date=\"1700000000\" subject=\"&quot;{RELEASE}.mkv&quot; yEnc (1/1)\">\
         <groups><group>alt.binaries.test</group></groups>\
         <segments><segment bytes=\"10\" number=\"1\">e2e@test.example</segment></segments>\
         </file></nzb>"
    )
}

async fn wait_until(what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..1000 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test]
async fn a_leftover_download_imports_into_the_library_through_app_state() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::connect(tmp.path().join("reelarr.db"))
        .await
        .unwrap();
    let mut options = DownloadOptions::new(tmp.path().join("downloads"));
    options.scheduler.poll_interval = Duration::from_millis(20);
    let worker = options.worker_config();
    // The production constructor: it builds and sets the real importer.
    let state = AppState::with_options(
        db,
        Arc::new(XChaChaSecretCipher::ephemeral().unwrap()),
        options,
    );

    // Library: one library folder (no default needed) and the movie.
    let library = tmp.path().join("library");
    std::fs::create_dir_all(&library).unwrap();
    let root = state
        .library_roots
        .add(NewLibraryRoot {
            path: library.to_string_lossy().into_owned(),
            kind: LibraryRootKind::Library,
            rescan_interval_secs: None,
        })
        .await
        .unwrap();
    let movie = SqliteMovieStore::new(&state.db)
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

    // A grab for that movie, admitted the way the job API does it, that
    // finished downloading before the last shutdown: the published folder is
    // recorded as its recovery path, staging is still on disk.
    let job = state
        .downloads
        .enqueue_nzb(
            nzb().into_bytes(),
            NewDownload {
                fallback_title: "fallback".into(),
                movie_id: Some(movie),
                ..NewDownload::default()
            },
        )
        .await
        .unwrap()
        .id;
    for to in [
        JobState::Downloading,
        JobState::Verifying,
        JobState::ReadyForImport,
    ] {
        state.jobs.transition(job, to, None).await.unwrap();
    }
    let published = worker.output_root.join(RELEASE);
    std::fs::create_dir_all(&published).unwrap();
    let video = published.join(format!("{RELEASE}.mkv"));
    std::fs::File::create(&video)
        .unwrap()
        .set_len(VIDEO_BYTES)
        .unwrap();
    let source_inode = std::fs::metadata(&video).unwrap().ino();
    let staging = worker.staging_root.join(job.to_string());
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("segment.bin"), b"staged").unwrap();
    SqliteRecoveryPaths::new((*state.jobs).clone())
        .set_recovery_path(job, Some(&published))
        .await
        .unwrap();

    // Start as the binary does. No NNTP provider is saved.
    assert!(state.nntp.load().await.unwrap().is_none());
    state.downloads.start().await.unwrap();
    let jobs = state.jobs.clone();
    wait_until("the job is Completed", async || {
        let now = jobs.get(job).await.unwrap();
        assert!(
            !matches!(now.state, JobState::Failed | JobState::Cancelled),
            "import did not complete: {:?} {:?}",
            now.state,
            now.failure
        );
        now.state == JobState::Completed
    })
    .await;
    let completed = state.jobs.get(job).await.unwrap();
    assert!(completed.failure.is_none());
    assert!(
        state.downloads.active().is_empty(),
        "no download ran on the way"
    );

    // The movie is in the library: one present file, renamed into the
    // movie's folder under the library root, the same bytes (hardlinked).
    let files = SqliteMediaFileStore::new(&state.db)
        .list_by_movie(movie)
        .await
        .unwrap();
    assert_eq!(files.len(), 1, "{files:?}");
    let file = &files[0];
    assert_eq!(file.status, MediaFileStatus::Present);
    assert_eq!(file.root_id, root.id);
    assert_eq!(file.size_bytes, VIDEO_BYTES);
    let path = Path::new(&file.path);
    assert!(
        path.starts_with(&library),
        "{path:?} is outside the library"
    );
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mkv"));
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(name.starts_with("The Example Movie (2004)"), "{name}");
    let meta = std::fs::metadata(path).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.len(), VIDEO_BYTES);
    assert_eq!(meta.ino(), source_inode, "hardlinked from the download");
    let ledger = SqliteImportLedger::new(&state.db)
        .get(&job.to_string())
        .await
        .unwrap()
        .expect("ledger row");
    assert_eq!(ledger.state, LedgerState::Done);
    assert_eq!(ledger.media_file_id, Some(file.id));

    // Source cleanup ran after Completed: staging and the published folder
    // are gone, the library copy stays.
    wait_until("source cleanup", async || {
        !published.exists() && !staging.exists()
    })
    .await;
    assert!(path.is_file());
    assert_eq!(std::fs::metadata(path).unwrap().nlink(), 1);

    state.downloads.shutdown().await;
}
