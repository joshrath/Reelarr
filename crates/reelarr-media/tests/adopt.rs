//! Adopting an existing (Radarr-style) library in place: files are linked to
//! movies and the filesystem is left exactly as it was.

use std::path::Path;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use reelarr_media::adopt::{MatchedBy, adopt_root};
use reelarr_media::import::matcher::normalize;
use reelarr_media::scan::{
    InMemoryMediaFileStore, MediaFileStore, ProbeError, Prober, ScanOptions, Scanner,
};
use reelarr_media::{InMemoryMovieStore, MetadataError, MetadataProvider, MovieStore, SearchQuery};
use reelarr_types::{
    Artwork, ExternalIds, LibraryRoot, LibraryRootId, LibraryRootKind, MediaProbe, MovieMetadata,
    MovieSearchResult,
};

struct FakeProber;

impl Prober for FakeProber {
    async fn probe(&self, _: &Path) -> Result<MediaProbe, ProbeError> {
        Ok(MediaProbe::default())
    }
}

fn meta(tmdb_id: u64, imdb: &str, title: &str, year: u16) -> MovieMetadata {
    MovieMetadata {
        external_ids: ExternalIds {
            tmdb_id,
            imdb_id: Some(imdb.into()),
        },
        title: title.into(),
        original_title: None,
        year: Some(year),
        overview: None,
        runtime_minutes: None,
        artwork: Artwork::default(),
    }
}

/// A small fixed catalogue. Search returns every movie whose title contains
/// the query, ignoring year, like a loose real search.
struct FakeProvider {
    catalogue: Vec<MovieMetadata>,
    calls: Mutex<Vec<String>>,
}

impl MetadataProvider for FakeProvider {
    async fn search_movies(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<MovieSearchResult>, MetadataError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("search {}", query.title));
        Ok(self
            .catalogue
            .iter()
            .filter(|m| normalize(&m.title).contains(&normalize(&query.title)))
            .map(|m| MovieSearchResult {
                tmdb_id: m.external_ids.tmdb_id,
                title: m.title.clone(),
                original_title: None,
                year: m.year,
                overview: None,
                poster_path: None,
            })
            .collect())
    }

    async fn movie_details(&self, tmdb_id: u64) -> Result<MovieMetadata, MetadataError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("details {tmdb_id}"));
        self.catalogue
            .iter()
            .find(|m| m.external_ids.tmdb_id == tmdb_id)
            .cloned()
            .ok_or(MetadataError::NotFound(tmdb_id))
    }
}

fn write(dir: &Path, rel: &str, bytes: &[u8]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Every entry under `dir` with its size and modification time.
fn snapshot(dir: &Path) -> Vec<(String, u64, u128)> {
    let mut out: Vec<_> = walkdir::WalkDir::new(dir)
        .into_iter()
        .map(|e| {
            let e = e.unwrap();
            let meta = e.metadata().unwrap();
            let modified = meta
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let len = if meta.is_file() { meta.len() } else { 0 };
            (e.path().to_string_lossy().into_owned(), len, modified)
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn adopts_a_radarr_library_without_touching_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write(
        dir,
        "The Matrix (1999) {tmdb-603}/The Matrix (1999) Bluray-1080p.mkv",
        b"m",
    );
    write(dir, "Heat (1995) [imdbid-tt0113277]/Heat.mkv", b"h");
    write(dir, "Alien (1979)/Alien.mkv", b"a");
    write(dir, "Alien (1979)/Alien.Directors.Cut.mkv", b"aa");
    write(dir, "Up (2009)/BDMV/index.bdmv", b"i");
    write(dir, "Up (2009)/BDMV/STREAM/00001.m2ts", b"u");
    write(dir, "Loose Movie 2004.mkv", b"l");
    write(dir, "Twins (1988)/Twins.mkv", b"t");
    write(dir, "Random Stuff/clip.mkv", b"r");
    write(dir, "Junk (2001)/notadisc.iso", b"nope");

    let root = LibraryRoot {
        id: LibraryRootId(1),
        path: dir.to_string_lossy().into_owned(),
        kind: LibraryRootKind::Library,
        rescan_interval_secs: None,
    };
    let scanner = Scanner::new(
        FakeProber,
        InMemoryMediaFileStore::new(),
        ScanOptions::default(),
    );
    assert_eq!(scanner.scan_root(&root).await.unwrap().added, 9);

    let movies = InMemoryMovieStore::new();
    let alien = movies
        .insert(meta(348, "tt0078748", "Alien", 1979), 0)
        .await
        .unwrap();
    let provider = FakeProvider {
        catalogue: vec![
            meta(603, "tt0133093", "The Matrix", 1999),
            meta(949, "tt0113277", "Heat", 1995),
            meta(14160, "tt1049413", "Up", 2009),
            meta(5000, "tt0000005", "Loose Movie", 2004),
            meta(9493, "tt0096320", "Twins", 1988),
            meta(9494, "tt0096321", "Twins", 1988),
        ],
        calls: Mutex::new(Vec::new()),
    };

    let before = snapshot(dir);
    let report = adopt_root(&movies, scanner.store(), &provider, root.id, dir)
        .await
        .unwrap();
    assert_eq!(snapshot(dir), before, "adoption must not write to disk");

    let by = |suffix: &str| {
        report
            .matched
            .iter()
            .find(|a| a.path.ends_with(suffix))
            .unwrap_or_else(|| panic!("{suffix} not matched: {report:?}"))
    };
    assert_eq!(by("Bluray-1080p.mkv").matched_by, MatchedBy::TmdbTag);
    assert_eq!(by("Heat.mkv").matched_by, MatchedBy::ImdbTag);
    assert_eq!(by("Alien.mkv").movie_id, alien.id);
    assert_eq!(
        by("Alien.Directors.Cut.mkv").matched_by,
        MatchedBy::LibraryTitle
    );
    assert_eq!(by("Up (2009)").matched_by, MatchedBy::MetadataSearch);
    assert_eq!(
        by("Loose Movie 2004.mkv").matched_by,
        MatchedBy::MetadataSearch
    );
    assert_eq!(report.matched.len(), 6);

    let reason = |suffix: &str| {
        report
            .unmatched
            .iter()
            .find(|u| u.path.ends_with(suffix))
            .map(|u| u.reason.clone())
            .unwrap()
    };
    assert!(reason("Twins.mkv").contains("possible matches"));
    assert!(reason("clip.mkv").contains("no title and year"));
    assert!(reason("notadisc.iso").contains("not a recognized disc image"));

    let mut titles: Vec<String> = movies
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.metadata.title)
        .collect();
    titles.sort();
    assert_eq!(titles, ["Alien", "Heat", "Loose Movie", "The Matrix", "Up"]);
    let file = scanner
        .store()
        .get(by("Heat.mkv").file_id)
        .await
        .unwrap()
        .unwrap();
    assert!(file.movie_id.is_some());

    // A second pass changes nothing and asks the provider only about the
    // files still unmatched.
    let calls_before = provider.calls.lock().unwrap().len();
    let again = adopt_root(&movies, scanner.store(), &provider, root.id, dir)
        .await
        .unwrap();
    assert!(again.matched.is_empty());
    assert_eq!(again.already_linked, 6);
    assert_eq!(again.unmatched.len(), 3);
    assert_eq!(provider.calls.lock().unwrap().len(), calls_before + 1);
    assert_eq!(snapshot(dir), before);
}
