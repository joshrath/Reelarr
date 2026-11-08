//! Import tests on real temporary folders with a fake prober.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reelarr_media::import::{
    ImportOptions, ImportOutcome, ImportRequest, Importer, TitleYearMatcher, TransferMode,
    Transferred,
};
use reelarr_media::scan::{InMemoryMediaFileStore, MediaFileStore, ProbeError, Prober};
use reelarr_media::{InMemoryMovieStore, MovieStore};
use reelarr_types::{
    Artwork, ExternalIds, LibraryRootId, MediaProbe, MediaStream, MovieId, MovieMetadata,
    StreamKind,
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

type TestImporter = Importer<
    Arc<InMemoryMovieStore>,
    Arc<InMemoryMediaFileStore>,
    FakeProber,
    TitleYearMatcher<Arc<InMemoryMovieStore>>,
>;

struct Env {
    _tmp: tempfile::TempDir,
    downloads: PathBuf,
    library: PathBuf,
    movies: Arc<InMemoryMovieStore>,
    movie: MovieId,
}

fn meta(tmdb_id: u64, title: &str, year: u16) -> MovieMetadata {
    MovieMetadata {
        external_ids: ExternalIds {
            tmdb_id,
            imdb_id: None,
        },
        title: title.into(),
        original_title: None,
        year: Some(year),
        overview: None,
        runtime_minutes: None,
        artwork: Artwork::default(),
    }
}

async fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let downloads = tmp.path().join("downloads");
    let library = tmp.path().join("library");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&library).unwrap();
    let movies = Arc::new(InMemoryMovieStore::new());
    let movie = movies
        .insert(meta(1001, "The Example Movie", 2004), 0)
        .await
        .unwrap()
        .id;
    Env {
        _tmp: tmp,
        downloads,
        library,
        movies,
        movie,
    }
}

fn importer(env: &Env, tweak: impl FnOnce(&mut ImportOptions)) -> TestImporter {
    let mut options = ImportOptions::new(&env.library, LibraryRootId(1));
    options.min_video_bytes = 10;
    tweak(&mut options);
    Importer::new(
        env.movies.clone(),
        Arc::new(InMemoryMediaFileStore::new()),
        FakeProber,
        TitleYearMatcher::new(env.movies.clone()),
        options,
    )
}

fn write(dir: &Path, rel: &str, len: usize) -> PathBuf {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, vec![b'x'; len]).unwrap();
    path
}

fn request(env: &Env, folder: &str, movie: Option<MovieId>) -> ImportRequest {
    ImportRequest {
        movie_id: movie,
        output_path: env.downloads.join(folder),
        release_title: folder.into(),
        category: Some("movies".into()),
    }
}

const JOB: &str = "The.Example.Movie.2004.1080p.BluRay.x264-GRP";

fn expected_dest(env: &Env, ext: &str) -> PathBuf {
    env.library
        .join("The Example Movie (2004)")
        .join(format!("The Example Movie (2004) [1080p].{ext}"))
}

#[tokio::test]
async fn hardlinks_main_file_renames_and_records_it() {
    let env = env().await;
    let main = write(&env.downloads, &format!("{JOB}/sub/a1b2c3d4.mkv"), 500);
    write(&env.downloads, &format!("{JOB}/sample-grp.mkv"), 600);
    write(&env.downloads, &format!("{JOB}/Sample/clip.mkv"), 700);
    write(&env.downloads, &format!("{JOB}/{JOB}.nfo"), 5);
    write(&env.downloads, &format!("{JOB}/sub/a1b2c3d4.en.srt"), 5);

    let imp = importer(&env, |_| {});
    let outcome = imp.import(&request(&env, JOB, Some(env.movie))).await;
    let ImportOutcome::Imported {
        media_file_id,
        path,
        transferred,
    } = outcome
    else {
        panic!("expected import, got {outcome:?}");
    };
    assert_eq!(path, expected_dest(&env, "mkv"));
    assert_eq!(transferred, Transferred::Hardlinked);
    assert!(main.exists(), "source kept");
    assert_eq!(
        std::fs::metadata(&main).unwrap().ino(),
        std::fs::metadata(&path).unwrap().ino()
    );
    assert!(
        path.with_file_name("The Example Movie (2004) [1080p].en.srt")
            .exists()
    );

    let stored = imp.files().get(media_file_id).await.unwrap().unwrap();
    assert_eq!(stored.movie_id, Some(env.movie));
    assert_eq!(stored.size_bytes, 500);
    assert!(stored.probe.is_some());
}

#[tokio::test]
async fn never_overwrites_an_existing_library_file() {
    let env = env().await;
    let main = write(&env.downloads, &format!("{JOB}/movie.mkv"), 500);
    let dest = expected_dest(&env, "mkv");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(&dest, b"existing").unwrap();

    let outcome = importer(&env, |_| {})
        .import(&request(&env, JOB, Some(env.movie)))
        .await;
    assert!(
        matches!(outcome, ImportOutcome::Failed { retryable: false, ref reason } if reason.contains("not overwriting")),
        "{outcome:?}"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"existing");
    assert!(main.exists());
}

#[tokio::test]
async fn copy_and_move_modes() {
    let env = env().await;
    let main = write(&env.downloads, &format!("{JOB}/movie.mp4"), 500);
    let copy = importer(&env, |o| o.mode = TransferMode::Copy)
        .import(&request(&env, JOB, Some(env.movie)))
        .await;
    assert!(matches!(
        copy,
        ImportOutcome::Imported {
            transferred: Transferred::Copied,
            ..
        }
    ));
    let dest = expected_dest(&env, "mp4");
    assert_ne!(
        std::fs::metadata(&main).unwrap().ino(),
        std::fs::metadata(&dest).unwrap().ino()
    );
    assert_eq!(std::fs::read(&dest).unwrap().len(), 500);
    assert!(main.exists());
    let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with(".reelarr-partial")
        })
        .collect();
    assert!(leftovers.is_empty());

    std::fs::remove_file(&dest).unwrap();
    let moved = importer(&env, |o| o.mode = TransferMode::Move)
        .import(&request(&env, JOB, Some(env.movie)))
        .await;
    assert!(matches!(
        moved,
        ImportOutcome::Imported {
            transferred: Transferred::Moved,
            ..
        }
    ));
    assert!(!main.exists(), "move removes the source");
    assert!(dest.exists());
}

#[tokio::test]
async fn matches_by_release_title_when_movie_id_is_missing() {
    let env = env().await;
    write(&env.downloads, &format!("{JOB}/movie.mkv"), 500);
    let outcome = importer(&env, |_| {})
        .import(&request(&env, JOB, None))
        .await;
    assert!(
        matches!(outcome, ImportOutcome::Imported { .. }),
        "{outcome:?}"
    );

    write(&env.downloads, "Unknown.Film.1999.720p/movie.mkv", 500);
    let outcome = importer(&env, |_| {})
        .import(&request(&env, "Unknown.Film.1999.720p", None))
        .await;
    assert_eq!(outcome, ImportOutcome::NoMatch);
}

#[tokio::test]
async fn ambiguous_match_is_no_match() {
    let env = env().await;
    env.movies
        .insert(meta(2002, "The Example Movie", 2004), 0)
        .await
        .unwrap();
    write(&env.downloads, &format!("{JOB}/movie.mkv"), 500);
    let outcome = importer(&env, |_| {})
        .import(&request(&env, JOB, None))
        .await;
    assert_eq!(outcome, ImportOutcome::NoMatch);
}

#[tokio::test]
async fn leftover_archives_fail_permanently() {
    let env = env().await;
    write(&env.downloads, &format!("{JOB}/movie.mkv"), 500);
    write(&env.downloads, &format!("{JOB}/movie.r00"), 50);
    let outcome = importer(&env, |_| {})
        .import(&request(&env, JOB, Some(env.movie)))
        .await;
    assert!(
        matches!(outcome, ImportOutcome::Failed { retryable: false, ref reason } if reason.contains("extraction incomplete")),
        "{outcome:?}"
    );
    assert!(!env.library.join("The Example Movie (2004)").exists());
}

#[tokio::test]
async fn only_samples_or_tiny_files_fail() {
    let env = env().await;
    write(&env.downloads, &format!("{JOB}/movie-sample.mkv"), 500);
    write(&env.downloads, &format!("{JOB}/tiny.mkv"), 3);
    let outcome = importer(&env, |_| {})
        .import(&request(&env, JOB, Some(env.movie)))
        .await;
    assert!(matches!(
        outcome,
        ImportOutcome::Failed {
            retryable: false,
            ..
        }
    ));
}

#[tokio::test]
async fn missing_output_is_retryable_and_unknown_movie_is_not() {
    let env = env().await;
    let outcome = importer(&env, |_| {})
        .import(&request(&env, "nope", Some(env.movie)))
        .await;
    assert!(matches!(
        outcome,
        ImportOutcome::Failed {
            retryable: true,
            ..
        }
    ));

    write(&env.downloads, &format!("{JOB}/movie.mkv"), 500);
    let outcome = importer(&env, |_| {})
        .import(&request(&env, JOB, Some(MovieId(999))))
        .await;
    assert!(matches!(
        outcome,
        ImportOutcome::Failed {
            retryable: false,
            ..
        }
    ));
}

#[tokio::test]
async fn custom_template_and_single_file_output() {
    let env = env().await;
    let file = write(&env.downloads, "loose/Some.Release.MKV", 500);
    let mut req = request(&env, "loose", Some(env.movie));
    req.output_path = file;
    let outcome = importer(&env, |o| {
        o.template = "Movies/{Title} [tmdb-{TmdbId}]".into()
    })
    .import(&req)
    .await;
    let ImportOutcome::Imported { path, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        path,
        env.library
            .join("Movies")
            .join("The Example Movie [tmdb-1001].mkv")
    );
}
