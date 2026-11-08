//! Matching a release title to a library movie when a download has no
//! `movie_id` (external or manual drops).
//!
//! This is a deliberately small title-and-year matcher. It sits behind
//! [`ReleaseMatcher`] so the release parser in `reelarr-parser` can replace
//! it once that lands.

use std::future::Future;

use reelarr_types::{Movie, MovieId};

use crate::store::{MovieStore, StoreError};

/// Finds the library movie a release belongs to.
pub trait ReleaseMatcher: Send + Sync {
    fn match_release(
        &self,
        release_title: &str,
    ) -> impl Future<Output = Result<Option<MovieId>, StoreError>> + Send;
}

/// Matches on normalized title plus year against movies in a [`MovieStore`].
#[derive(Debug)]
pub struct TitleYearMatcher<S> {
    store: S,
}

impl<S> TitleYearMatcher<S> {
    /// Pass a shared handle (e.g. `Arc<Store>`) to reuse the importer's store.
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

impl<S: MovieStore> ReleaseMatcher for TitleYearMatcher<S> {
    async fn match_release(&self, release_title: &str) -> Result<Option<MovieId>, StoreError> {
        let Some((title, year)) = split_title_year(release_title) else {
            return Ok(None);
        };
        let movies = self.store.list().await?;
        let matches: Vec<&Movie> = movies
            .iter()
            .filter(|m| m.metadata.year == Some(year))
            .filter(|m| {
                normalize(&m.metadata.title) == title
                    || m.metadata
                        .original_title
                        .as_deref()
                        .is_some_and(|t| normalize(t) == title)
            })
            .collect();
        // Ambiguous matches are left for manual import.
        Ok(match matches.as_slice() {
            [one] => Some(one.id),
            _ => None,
        })
    }
}

/// Lowercase alphanumeric words joined by single spaces; `&` becomes `and`.
pub fn normalize(value: &str) -> String {
    value
        .to_lowercase()
        .replace('&', " and ")
        .replace('\'', "")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Split `Some.Movie.2010.1080p.WEB` into (`some movie`, 2010). Uses the last
/// plausible year so titles containing a year (e.g. `2001 A Space ...`) work.
pub fn split_title_year(release: &str) -> Option<(String, u16)> {
    let words: Vec<&str> = release
        .split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '&')
        .filter(|w| !w.is_empty())
        .collect();
    let pos = words.iter().enumerate().skip(1).rev().find_map(|(i, w)| {
        let year: u16 = w.parse().ok()?;
        (w.len() == 4 && (1888..=2100).contains(&year)).then_some((i, year))
    })?;
    let title = normalize(&words[..pos.0].join(" "));
    (!title.is_empty()).then_some((title, pos.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_release_names() {
        assert_eq!(
            split_title_year("The.Example.Movie.2004.1080p.BluRay.x264-GRP"),
            Some(("the example movie".into(), 2004))
        );
        assert_eq!(
            split_title_year("2001 A Space Story (1968) [1080p]"),
            Some(("2001 a space story".into(), 1968))
        );
        assert_eq!(
            split_title_year("Tom & Jerry's.Movie.1999"),
            Some(("tom and jerrys movie".into(), 1999))
        );
        assert_eq!(split_title_year("NoYearHere.1080p"), None);
        assert_eq!(split_title_year("2010"), None);
    }
}
