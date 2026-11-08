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
  and place this notice prominently: "This application uses TMDB and the TMDB
  APIs but is not endorsed, certified, or otherwise approved by TMDB." This is
  tracked for the UI in #19.
- TMDB content may not be used with machine-learning or AI applications, and
  apps must not use excessive bandwidth or imply that TMDB is their source of
  origin.
- TMDB data may not be cached for more than six months, and must be deleted
  if API access ends. `Movie::metadata_fetched_at` records when data was
  fetched, and refresh and purge are tracked in #20.
- TMDB may rate-limit requests. HTTP 429 is returned as
  `MetadataError::RateLimited` with the `Retry-After` value, when it is present.

## Library scanning

`scan::Scanner::scan_root` walks a configured `LibraryRoot`, which can be a
library folder, a network share or a manual-import folder. It records video
files in a `MediaFileStore`. The SQLite implementation lives in `reelarr-db`,
and `InMemoryMediaFileStore` is for tests.

- **Probing.** `FfprobeProber` runs `ffprobe -v error -print_format json
  -show_format -show_streams` as a separate process, with a timeout and
  `kill_on_drop`. It reads the container, duration, bit rate and, for each
  stream, the codec, profile, resolution, channels, language and the
  default and forced flags. `ScanOptions::probe_concurrency` (default 2)
  limits how many probes run at once, which matters on slow shares.
- **Change detection.** A file is re-probed only when its size or mtime
  changes, or when an earlier probe failed.
- **Missing files.** A file that disappears is marked `Missing { since }`.
  It is never deleted, and it is restored if it comes back unchanged.
- **Unmounted shares.** If the root path is missing, nothing is marked
  missing (`RootUnavailable`). The same goes for a root that is reachable
  but contains no video files while the library knows some
  (`RootLooksUnmounted`).
- **What gets skipped.** Hidden files and folders, `.reelarr-partial` import
  temporaries and anything that isn't a video extension. Symlinks are not
  followed.
- **Periodic rescans.** `scan::run_periodic` polls each root on its
  `rescan_interval_secs` (default 6 hours, configurable per root, `None`
  disables it) until it is told to shut down. Polling is used because
  inotify-style notifications are unreliable on SMB and NFS. Only one scan
  per root runs at a time.
