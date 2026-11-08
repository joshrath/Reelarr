# reelarr-media

Library scanning, import, rename and metadata for Reelarr.

## Movie metadata

- `MetadataProvider` is the trait the rest of Reelarr uses to search and look
  up movies, so the source can be swapped or mocked.
- `metadata::tmdb::TmdbProvider` implements it against TMDB's documented v3
  API (`/3/search/movie`, `/3/movie/{id}`).
- `MovieStore` is the persistence trait. `InMemoryMovieStore` is for tests.
  The SQLite implementation and migrations live in `reelarr-db`.
- `MovieService` ties the two together: `search` and `add_movie(tmdb_id)`.
  Adding a movie that is already in the library returns
  `AddMovieError::AlreadyExists` without calling TMDB.

Artwork is stored as TMDB-relative paths (for example `/abc.jpg`).
`TmdbProvider::image_url("w500", path)` builds the full URL. Images are not
downloaded.

### Configuration

Set `REELARR_TMDB_API_KEY` to either a TMDB v4 API Read Access Token (sent as
a bearer token) or a v3 API key (sent as `api_key`). Reelarr does not ship a
key. Every install uses its own, from <https://www.themoviedb.org/settings/api>.
Never commit a key. The tests use local mock servers and fixtures, and make no
live calls.

### TMDB terms and attribution

From the [TMDB API Terms of Use](https://www.themoviedb.org/api-terms-of-use)
and [developer FAQ](https://developer.themoviedb.org/docs/faq):

- Free use is for non-commercial purposes. Commercial use needs a separate
  agreement with TMDB.
- Apps must show the TMDB logo, less prominent than the app's own branding,
  and this notice: "This product uses the TMDB API but is not endorsed or
  certified by TMDB." This is tracked for the UI in #19.
- TMDB data may not be cached for more than six months, and must be deleted
  if API access ends. `Movie::metadata_fetched_at` records when data was
  fetched, and refresh and purge are tracked in #20.
- TMDB may rate-limit requests. HTTP 429 is returned as
  `MetadataError::RateLimited` with the `Retry-After` value, when it is present.
