//! SQLite stores, root health and the playback lookup.

use std::path::Path;

use reelarr_db::Database;
use reelarr_media::db::{
    DbMediaLookup, RootError, SqliteLibraryRootStore, SqliteMediaFileStore, SqliteMovieStore,
};
use reelarr_media::health::{Checked, HealthChecker, RootScan, check_path, scan_checked};
use reelarr_media::scan::{MediaFileStore, NewMediaFile, ProbeError, Prober, ScanOptions, Scanner};
use reelarr_media::store::{MovieStore, StoreError};
use reelarr_stream::{Lookup, MediaLookup};
use reelarr_types::{
    Artwork, ExternalIds, LibraryRoot, LibraryRootKind, MediaFileId, MediaFileStatus, MediaKind,
    MediaProbe, MovieMetadata, NewLibraryRoot, RootHealth,
};

/// A file-backed database: `:memory:` gives each pooled connection its own
/// empty database.
async fn db() -> Database {
    let dir = tempfile::tempdir().unwrap().keep();
    Database::connect(dir.join("reelarr.db")).await.unwrap()
}

fn metadata(tmdb_id: u64, title: &str) -> MovieMetadata {
    MovieMetadata {
        external_ids: ExternalIds {
            tmdb_id,
            imdb_id: Some("tt0111161".into()),
        },
        title: title.into(),
        original_title: None,
        year: Some(1994),
        overview: Some("Two imprisoned men bond.".into()),
        runtime_minutes: Some(142),
        artwork: Artwork {
            poster_path: Some("/p.jpg".into()),
            backdrop_path: None,
        },
    }
}

fn new_root(path: &Path) -> NewLibraryRoot {
    NewLibraryRoot {
        path: path.to_string_lossy().into_owned(),
        kind: LibraryRootKind::Library,
        rescan_interval_secs: None,
    }
}

fn probe() -> MediaProbe {
    MediaProbe {
        container: vec!["matroska".into()],
        duration_ms: Some(1000),
        ..MediaProbe::default()
    }
}

#[tokio::test]
async fn movies_round_trip_and_reject_duplicate_tmdb_ids() {
    let db = db().await;
    let store = SqliteMovieStore::new(&db);
    let movie = store
        .insert(metadata(278, "The Shawshank Redemption"), 100)
        .await
        .unwrap();
    assert_eq!(store.get(movie.id).await.unwrap(), Some(movie.clone()));
    assert_eq!(
        store.find_by_tmdb_id(278).await.unwrap(),
        Some(movie.clone())
    );
    match store.insert(metadata(278, "Again"), 200).await {
        Err(StoreError::Duplicate { tmdb_id, existing }) => {
            assert_eq!((tmdb_id, existing), (278, movie.id));
        }
        other => panic!("expected duplicate, got {other:?}"),
    }
    store
        .insert(metadata(680, "Pulp Fiction"), 100)
        .await
        .unwrap();
    assert_eq!(store.list().await.unwrap().len(), 2);
}

#[tokio::test]
async fn media_files_round_trip_status_and_probe() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = SqliteLibraryRootStore::new(&db)
        .add(new_root(tmp.path()))
        .await
        .unwrap();
    let store = SqliteMediaFileStore::new(&db);
    let path = format!("{}/a.mkv", root.path);
    let mut file = store
        .insert(NewMediaFile {
            movie_id: None,
            root_id: root.id,
            path: path.clone(),
            size_bytes: 5_000_000_000,
            modified_at: 42,
            probe: Some(probe()),
            kind: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(store.get(file.id).await.unwrap(), Some(file.clone()));
    assert!(
        store
            .insert(NewMediaFile {
                movie_id: None,
                root_id: root.id,
                path: path.clone(),
                size_bytes: 1,
                modified_at: 1,
                probe: None,
                kind: Default::default(),
            })
            .await
            .is_err()
    );
    file.status = MediaFileStatus::Missing { since: 99 };
    file.probe = None;
    store.update(&file).await.unwrap();
    assert_eq!(store.get_by_path(&path).await.unwrap(), Some(file.clone()));
    assert_eq!(store.list_by_root(root.id).await.unwrap(), vec![file]);
}

#[tokio::test]
async fn roots_reject_relative_duplicate_and_nested_paths() {
    let db = db().await;
    let store = SqliteLibraryRootStore::new(&db);
    let tmp = tempfile::tempdir().unwrap();
    let root = store.add(new_root(tmp.path())).await.unwrap();
    assert!(matches!(
        store.add(new_root(Path::new("movies"))).await,
        Err(RootError::InvalidPath(_))
    ));
    assert!(matches!(
        store.add(new_root(Path::new("/data/../etc"))).await,
        Err(RootError::InvalidPath(_))
    ));
    let mut trailing = root.path.clone();
    trailing.push('/');
    assert!(matches!(
        store.add(new_root(Path::new(&trailing))).await,
        Err(RootError::Duplicate { .. })
    ));
    assert!(matches!(
        store.add(new_root(&tmp.path().join("sub"))).await,
        Err(RootError::Overlaps { .. })
    ));
    assert!(matches!(
        store.add(new_root(tmp.path().parent().unwrap())).await,
        Err(RootError::Overlaps { .. })
    ));
    assert_eq!(store.list().await.unwrap(), vec![root]);
}

#[tokio::test]
async fn removing_a_root_with_files_needs_confirmation_and_keeps_disk_files() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.mkv"), b"x").unwrap();
    let roots = SqliteLibraryRootStore::new(&db);
    let files = SqliteMediaFileStore::new(&db);
    let root = roots.add(new_root(tmp.path())).await.unwrap();
    files
        .insert(NewMediaFile {
            movie_id: None,
            root_id: root.id,
            path: format!("{}/a.mkv", root.path),
            size_bytes: 1,
            modified_at: 1,
            probe: None,
            kind: Default::default(),
        })
        .await
        .unwrap();
    assert!(matches!(
        roots.remove(root.id, false).await,
        Err(RootError::HasFiles { count: 1 })
    ));
    assert_eq!(roots.list_info().await.unwrap()[0].file_count, 1);
    roots.remove(root.id, true).await.unwrap();
    assert!(roots.list().await.unwrap().is_empty());
    assert!(files.list_by_root(root.id).await.unwrap().is_empty());
    assert!(tmp.path().join("a.mkv").exists());
    assert!(matches!(
        roots.remove(root.id, true).await,
        Err(RootError::NotFound)
    ));
}

#[tokio::test]
async fn offline_keeps_its_first_since_until_the_root_recovers() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let roots = SqliteLibraryRootStore::new(&db);
    let root = roots.add(new_root(tmp.path())).await.unwrap();
    assert_eq!(
        roots.health(root.id).await.unwrap(),
        Some(RootHealth::Unknown)
    );
    let offline = |since| RootHealth::Offline {
        since,
        reason: "share down".into(),
    };
    roots.set_health(root.id, &offline(10), 10).await.unwrap();
    roots.set_health(root.id, &offline(20), 20).await.unwrap();
    assert_eq!(roots.health(root.id).await.unwrap(), Some(offline(10)));
    assert_eq!(
        roots.list_info().await.unwrap()[0].health_checked_at,
        Some(20)
    );
    roots
        .set_health(root.id, &RootHealth::Online, 30)
        .await
        .unwrap();
    roots.set_health(root.id, &offline(40), 40).await.unwrap();
    assert_eq!(roots.health(root.id).await.unwrap(), Some(offline(40)));
}

#[tokio::test]
async fn lookup_distinguishes_found_missing_and_unavailable() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let roots = SqliteLibraryRootStore::new(&db);
    let files = SqliteMediaFileStore::new(&db);
    let root = roots.add(new_root(tmp.path())).await.unwrap();
    let mut file = files
        .insert(NewMediaFile {
            movie_id: None,
            root_id: root.id,
            path: format!("{}/a.mkv", root.path),
            size_bytes: 1,
            modified_at: 1,
            probe: Some(probe()),
            kind: Default::default(),
        })
        .await
        .unwrap();
    let lookup = DbMediaLookup::new(&db);
    match lookup.lookup(file.id).await.unwrap() {
        Lookup::Found(found) => {
            assert_eq!(found.path, Path::new(&file.path));
            assert_eq!(found.root, Path::new(&root.path));
            assert_eq!(found.probe, Some(probe()));
        }
        other => panic!("expected found, got {other:?}"),
    }
    assert_eq!(
        lookup.lookup(MediaFileId(999)).await.unwrap(),
        Lookup::NotFound
    );
    let disc = files
        .insert(NewMediaFile {
            movie_id: None,
            root_id: root.id,
            path: format!("{}/Movie (2010)", root.path),
            size_bytes: 9,
            modified_at: 1,
            probe: None,
            kind: MediaKind::DiscFolder,
        })
        .await
        .unwrap();
    assert_eq!(
        files.get(disc.id).await.unwrap().unwrap().kind,
        MediaKind::DiscFolder
    );
    match lookup.lookup(disc.id).await.unwrap() {
        Lookup::Found(found) => assert_eq!(found.kind, MediaKind::DiscFolder),
        other => panic!("expected found, got {other:?}"),
    }
    roots
        .set_health(
            root.id,
            &RootHealth::Offline {
                since: 1,
                reason: "NAS unreachable".into(),
            },
            1,
        )
        .await
        .unwrap();
    assert_eq!(
        lookup.lookup(file.id).await.unwrap(),
        Lookup::Unavailable {
            reason: "NAS unreachable".into()
        }
    );
    roots
        .set_health(root.id, &RootHealth::Online, 2)
        .await
        .unwrap();
    file.status = MediaFileStatus::Missing { since: 3 };
    files.update(&file).await.unwrap();
    assert_eq!(lookup.lookup(file.id).await.unwrap(), Lookup::NotFound);
}

#[test]
fn check_path_reports_missing_and_unmounted_folders() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(check_path(tmp.path(), 0), Checked::Online);
    assert!(matches!(check_path(tmp.path(), 3), Checked::Offline(r) if r.contains("not mounted")));
    assert!(matches!(
        check_path(&tmp.path().join("gone"), 0),
        Checked::Offline(r) if r.contains("not found")
    ));
    let file = tmp.path().join("f");
    std::fs::write(&file, b"x").unwrap();
    assert!(matches!(check_path(&file, 0), Checked::Offline(r) if r.contains("not a folder")));
}

struct OkProber;

impl Prober for OkProber {
    async fn probe(&self, _: &Path) -> Result<MediaProbe, ProbeError> {
        Ok(probe())
    }
}

#[tokio::test]
async fn unmounted_root_is_skipped_marked_offline_then_recovers() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("share");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("Movie (2020).mkv"), vec![0u8; 2048]).unwrap();
    let roots = SqliteLibraryRootStore::new(&db);
    let root: LibraryRoot = roots.add(new_root(&dir)).await.unwrap();
    let scanner = Scanner::new(
        OkProber,
        SqliteMediaFileStore::new(&db),
        ScanOptions::default(),
    );
    let checker = HealthChecker::default();

    let first = scan_checked(&scanner, &roots, &checker, &root)
        .await
        .unwrap();
    assert!(matches!(first, RootScan::Scanned(_)), "{first:?}");
    assert_eq!(
        roots.health(root.id).await.unwrap(),
        Some(RootHealth::Online)
    );
    let known = scanner.store().list_by_root(root.id).await.unwrap();
    assert_eq!(known.len(), 1);

    // Unmount: the folder disappears.
    std::fs::rename(&dir, tmp.path().join("away")).unwrap();
    let skipped = scan_checked(&scanner, &roots, &checker, &root)
        .await
        .unwrap();
    assert!(
        matches!(skipped, RootScan::Skipped(RootHealth::Offline { .. })),
        "{skipped:?}"
    );
    assert_eq!(scanner.store().list_by_root(root.id).await.unwrap(), known);
    assert!(matches!(
        DbMediaLookup::new(&db).lookup(known[0].id).await.unwrap(),
        Lookup::Unavailable { .. }
    ));

    // Mount point present but empty.
    std::fs::create_dir(&dir).unwrap();
    let skipped = scan_checked(&scanner, &roots, &checker, &root)
        .await
        .unwrap();
    assert!(matches!(skipped, RootScan::Skipped(_)), "{skipped:?}");
    assert_eq!(scanner.store().list_by_root(root.id).await.unwrap(), known);

    // Reconnect.
    std::fs::remove_dir(&dir).unwrap();
    std::fs::rename(tmp.path().join("away"), &dir).unwrap();
    let back = scan_checked(&scanner, &roots, &checker, &root)
        .await
        .unwrap();
    assert!(matches!(back, RootScan::Scanned(_)), "{back:?}");
    assert_eq!(
        roots.health(root.id).await.unwrap(),
        Some(RootHealth::Online)
    );
    assert!(matches!(
        DbMediaLookup::new(&db).lookup(known[0].id).await.unwrap(),
        Lookup::Found(_)
    ));
}
