-- Movie library: movies, library roots (local or network shares) and the
-- media files found under them. Files are never deleted because a root is
-- unreachable; root health is tracked separately and missing files keep their
-- row with missing_since set.
CREATE TABLE movies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tmdb_id INTEGER NOT NULL UNIQUE CHECK (tmdb_id >= 0),
    imdb_id TEXT,
    title TEXT NOT NULL,
    original_title TEXT,
    year INTEGER CHECK (year IS NULL OR year BETWEEN 0 AND 65535),
    overview TEXT,
    runtime_minutes INTEGER CHECK (runtime_minutes IS NULL OR runtime_minutes >= 0),
    poster_path TEXT,
    backdrop_path TEXT,
    metadata_fetched_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE library_roots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('library', 'manual_import')),
    -- NULL disables periodic rescans.
    rescan_interval_secs INTEGER CHECK (rescan_interval_secs IS NULL OR rescan_interval_secs > 0),
    health TEXT NOT NULL DEFAULT 'unknown'
        CHECK (health IN ('unknown', 'online', 'offline', 'permission_denied')),
    health_reason TEXT,
    health_since INTEGER,
    health_checked_at INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE media_files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    movie_id INTEGER REFERENCES movies(id) ON DELETE SET NULL,
    -- RESTRICT: removing a root must explicitly forget its files first.
    root_id INTEGER NOT NULL REFERENCES library_roots(id) ON DELETE RESTRICT,
    path TEXT NOT NULL UNIQUE,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    modified_at INTEGER NOT NULL,
    missing_since INTEGER,
    -- ffprobe summary as JSON (reelarr_types::MediaProbe).
    probe TEXT
);
CREATE INDEX media_files_root_idx ON media_files(root_id);
CREATE INDEX media_files_movie_idx ON media_files(movie_id);
