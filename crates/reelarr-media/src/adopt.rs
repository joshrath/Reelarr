//! Adopting an existing movie library in place.
//!
//! Folders organized by Radarr or by hand (`Movie Title (2010)/...`,
//! optionally tagged `{tmdb-603}` or `[imdbid-tt0133093]`) are linked to
//! movies without touching the disk: adoption only sets `movie_id` on media
//! files the scanner already recorded. Nothing is renamed, moved or written.
//!
//! Matching order for each file:
//! 1. A TMDB or IMDb id tag in the folder (or file) name.
//! 2. Title and year from the folder name (or the file name for files at the
//!    top of the root), against movies already in the library.
//! 3. The same title and year against the metadata provider, accepted only
//!    when exactly one result has that exact title and year.
//!
//! Anything else is reported as unmatched with a reason, for manual review.

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use reelarr_types::{LibraryRootId, MediaFileId, MediaFileStatus, MediaKind, Movie, MovieId};

use crate::import::matcher::{normalize, split_title_year};
use crate::metadata::{MetadataProvider, SearchQuery};
use crate::scan::MediaFileStore;
use crate::store::{MovieStore, StoreError};

/// How a file was linked to its movie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedBy {
    TmdbTag,
    ImdbTag,
    /// Title and year matched a movie already in the library.
    LibraryTitle,
    /// Title and year matched exactly one metadata search result.
    MetadataSearch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    pub file_id: MediaFileId,
    pub path: String,
    pub movie_id: MovieId,
    pub matched_by: MatchedBy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmatched {
    pub file_id: MediaFileId,
    pub path: String,
    pub reason: String,
}

/// What an adoption pass did.
#[derive(Debug, Default)]
pub struct AdoptReport {
    pub matched: Vec<Adopted>,
    /// Files that already belonged to a movie and were left alone.
    pub already_linked: usize,
    pub unmatched: Vec<Unmatched>,
}

/// An id tag found in a folder or file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdTag {
    Tmdb(u64),
    Imdb(String),
}

/// Find `{tmdb-603}`, `[tmdbid-603]`, `{imdb-tt0133093}` or
/// `[imdbid-tt0133093]` (case-insensitive; `=` also accepted) in `name`.
/// A TMDB tag wins over an IMDb tag.
pub fn find_id_tag(name: &str) -> Option<IdTag> {
    let mut imdb = None;
    for inner in bracketed(name) {
        let inner = inner.trim().to_ascii_lowercase();
        let Some((key, value)) = inner.split_once(['-', '=']) else {
            continue;
        };
        match key {
            "tmdb" | "tmdbid" => {
                if let Ok(id) = value.parse::<u64>() {
                    return Some(IdTag::Tmdb(id));
                }
            }
            "imdb" | "imdbid" => {
                let digits = value.strip_prefix("tt").unwrap_or("");
                if digits.len() >= 7 && digits.bytes().all(|b| b.is_ascii_digit()) {
                    imdb.get_or_insert(IdTag::Imdb(format!("tt{digits}")));
                }
            }
            _ => {}
        }
    }
    imdb
}

/// Contents of every `{...}` and `[...]` segment.
fn bracketed(name: &str) -> impl Iterator<Item = &str> {
    let mut rest = name;
    std::iter::from_fn(move || {
        loop {
            let start = rest.find(['{', '['])?;
            let close = if rest.as_bytes()[start] == b'{' {
                '}'
            } else {
                ']'
            };
            let after = &rest[start + 1..];
            match after.find(close) {
                Some(end) => {
                    rest = &after[end + 1..];
                    return Some(&after[..end]);
                }
                None => rest = after,
            }
        }
    })
}

/// Remove `{...}` and `[...]` segments so tags and quality notes don't end
/// up in the title.
fn strip_bracketed(name: &str) -> String {
    let mut out = name.to_owned();
    for inner in bracketed(name) {
        for (open, close) in [('{', '}'), ('[', ']')] {
            out = out.replace(&format!("{open}{inner}{close}"), " ");
        }
    }
    out
}

/// The name that identifies a file's movie: the disc folder itself, the
/// containing folder, or the file stem for files directly in the root.
fn identifying_name(path: &Path, kind: MediaKind, root: &Path) -> Option<String> {
    let name = if kind == MediaKind::DiscFolder {
        path.file_name()?
    } else {
        match path.parent() {
            Some(parent) if parent != root => parent.file_name()?,
            _ => path.file_stem()?,
        }
    };
    Some(name.to_string_lossy().into_owned())
}

/// Link the unlinked media files of one root to movies, without writing to
/// the filesystem. Store failures abort the pass; metadata failures leave a
/// file unmatched with the error as its reason.
pub async fn adopt_root<M, F, P>(
    movies: &M,
    files: &F,
    provider: &P,
    root_id: LibraryRootId,
    root_path: &Path,
) -> Result<AdoptReport, StoreError>
where
    M: MovieStore,
    F: MediaFileStore,
    P: MetadataProvider,
{
    let mut report = AdoptReport::default();
    let mut library = movies.list().await?;
    // One resolution per identifying name, so a folder with several files
    // costs one metadata lookup.
    let mut resolved: HashMap<String, Result<(MovieId, MatchedBy), String>> = HashMap::new();

    for mut file in files.list_by_root(root_id).await? {
        if file.movie_id.is_some() {
            report.already_linked += 1;
            continue;
        }
        let unmatched = |reason: String| Unmatched {
            file_id: file.id,
            path: file.path.clone(),
            reason,
        };
        if matches!(file.status, MediaFileStatus::Missing { .. }) {
            report
                .unmatched
                .push(unmatched("file is missing on disk".into()));
            continue;
        }
        if file.kind == MediaKind::UnrecognizedImage {
            report
                .unmatched
                .push(unmatched("not a recognized disc image".into()));
            continue;
        }
        let path = Path::new(&file.path);
        let Some(name) = identifying_name(path, file.kind, root_path) else {
            report
                .unmatched
                .push(unmatched("no folder or file name to match".into()));
            continue;
        };
        // A tag on the file itself also counts when the folder has none.
        let file_tag = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(find_id_tag);
        let key = format!("{name}\u{0}{file_tag:?}");
        let outcome = match resolved.get(&key) {
            Some(outcome) => outcome.clone(),
            None => {
                let outcome =
                    resolve(movies, provider, &mut library, &name, file_tag.clone()).await?;
                resolved.insert(key, outcome.clone());
                outcome
            }
        };
        match outcome {
            Ok((movie_id, matched_by)) => {
                file.movie_id = Some(movie_id);
                files.update(&file).await?;
                report.matched.push(Adopted {
                    file_id: file.id,
                    path: file.path,
                    movie_id,
                    matched_by,
                });
            }
            Err(reason) => report.unmatched.push(unmatched(reason)),
        }
    }
    Ok(report)
}

async fn resolve<M: MovieStore, P: MetadataProvider>(
    movies: &M,
    provider: &P,
    library: &mut Vec<Movie>,
    name: &str,
    file_tag: Option<IdTag>,
) -> Result<Result<(MovieId, MatchedBy), String>, StoreError> {
    let title_year = split_title_year(&strip_bracketed(name));
    match find_id_tag(name).or(file_tag) {
        Some(IdTag::Tmdb(tmdb_id)) => {
            if let Some(m) = library
                .iter()
                .find(|m| m.metadata.external_ids.tmdb_id == tmdb_id)
            {
                return Ok(Ok((m.id, MatchedBy::TmdbTag)));
            }
            return Ok(add_by_tmdb(movies, provider, library, tmdb_id)
                .await?
                .map(|id| (id, MatchedBy::TmdbTag)));
        }
        Some(IdTag::Imdb(imdb_id)) => {
            if let Some(m) = library
                .iter()
                .find(|m| m.metadata.external_ids.imdb_id.as_deref() == Some(imdb_id.as_str()))
            {
                return Ok(Ok((m.id, MatchedBy::ImdbTag)));
            }
            let Some((title, year)) = &title_year else {
                return Ok(Err(format!(
                    "IMDb tag {imdb_id} needs a title and year in the name to look up"
                )));
            };
            let candidates = match search_exact(provider, title, *year).await {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            for tmdb_id in candidates.into_iter().take(5) {
                match provider.movie_details(tmdb_id).await {
                    Ok(meta) if meta.external_ids.imdb_id.as_deref() == Some(imdb_id.as_str()) => {
                        return Ok(insert(movies, library, meta)
                            .await?
                            .map(|id| (id, MatchedBy::ImdbTag)));
                    }
                    Ok(_) => {}
                    Err(e) => return Ok(Err(format!("metadata lookup failed: {e}"))),
                }
            }
            return Ok(Err(format!("no TMDB movie found for IMDb id {imdb_id}")));
        }
        None => {}
    }

    let Some((title, year)) = title_year else {
        return Ok(Err(format!("no title and year in \"{name}\"")));
    };
    let local: Vec<&Movie> = library
        .iter()
        .filter(|m| {
            m.metadata.year == Some(year)
                && (normalize(&m.metadata.title) == title
                    || m.metadata
                        .original_title
                        .as_deref()
                        .is_some_and(|t| normalize(t) == title))
        })
        .collect();
    match local.as_slice() {
        [one] => return Ok(Ok((one.id, MatchedBy::LibraryTitle))),
        [] => {}
        _ => {
            return Ok(Err(format!(
                "\"{title}\" ({year}) matches several library movies"
            )));
        }
    }
    let candidates = match search_exact(provider, &title, year).await {
        Ok(c) => c,
        Err(e) => return Ok(Err(e)),
    };
    match candidates.as_slice() {
        [tmdb_id] => Ok(add_by_tmdb(movies, provider, library, *tmdb_id)
            .await?
            .map(|id| (id, MatchedBy::MetadataSearch))),
        [] => Ok(Err(format!("no exact match for \"{title}\" ({year})"))),
        _ => Ok(Err(format!(
            "\"{title}\" ({year}) has {} possible matches",
            candidates.len()
        ))),
    }
}

/// TMDB ids of search results whose title (or original title) and year match
/// exactly after normalization.
async fn search_exact<P: MetadataProvider>(
    provider: &P,
    title: &str,
    year: u16,
) -> Result<Vec<u64>, String> {
    let results = provider
        .search_movies(&SearchQuery {
            title: title.to_owned(),
            year: Some(year),
        })
        .await
        .map_err(|e| format!("metadata search failed: {e}"))?;
    let mut ids: Vec<u64> = results
        .into_iter()
        .filter(|r| {
            r.year == Some(year)
                && (normalize(&r.title) == title
                    || r.original_title
                        .as_deref()
                        .is_some_and(|t| normalize(t) == title))
        })
        .map(|r| r.tmdb_id)
        .collect();
    ids.dedup();
    Ok(ids)
}

async fn add_by_tmdb<M: MovieStore, P: MetadataProvider>(
    movies: &M,
    provider: &P,
    library: &mut Vec<Movie>,
    tmdb_id: u64,
) -> Result<Result<MovieId, String>, StoreError> {
    match provider.movie_details(tmdb_id).await {
        Ok(meta) => insert(movies, library, meta).await,
        Err(e) => Ok(Err(format!(
            "metadata lookup for TMDB {tmdb_id} failed: {e}"
        ))),
    }
}

async fn insert<M: MovieStore>(
    movies: &M,
    library: &mut Vec<Movie>,
    meta: reelarr_types::MovieMetadata,
) -> Result<Result<MovieId, String>, StoreError> {
    match movies.insert(meta, unix_now()).await {
        Ok(movie) => {
            let id = movie.id;
            library.push(movie);
            Ok(Ok(id))
        }
        Err(StoreError::Duplicate { existing, .. }) => Ok(Ok(existing)),
        Err(e) => Err(e),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_id_tags() {
        assert_eq!(
            find_id_tag("The Matrix (1999) {tmdb-603}"),
            Some(IdTag::Tmdb(603))
        );
        assert_eq!(
            find_id_tag("The Matrix (1999) [tmdbid-603]"),
            Some(IdTag::Tmdb(603))
        );
        assert_eq!(
            find_id_tag("The Matrix (1999) [imdbid-tt0133093]"),
            Some(IdTag::Imdb("tt0133093".into()))
        );
        assert_eq!(
            find_id_tag("X (1999) {IMDB-tt0133093} {tmdb-603}"),
            Some(IdTag::Tmdb(603))
        );
        assert_eq!(find_id_tag("Movie (2010) [1080p]"), None);
        assert_eq!(find_id_tag("Movie {imdb-tt12}"), None);
    }

    #[test]
    fn strips_tags_before_reading_the_year() {
        assert_eq!(
            split_title_year(&strip_bracketed("Heat (1995) {tmdb-949} [Bluray-1080p]")),
            Some(("heat".into(), 1995))
        );
    }
}
