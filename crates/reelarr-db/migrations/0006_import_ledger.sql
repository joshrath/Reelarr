-- Per-job import ledger (#29). A pending row is written before any file is
-- touched, so a repeated import of the same job after a crash can tell its
-- own half-finished work from someone else's file.
CREATE TABLE import_ledger (
    job_id TEXT PRIMARY KEY NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'done')),
    movie_id INTEGER NOT NULL REFERENCES movies(id) ON DELETE CASCADE,
    root_id INTEGER NOT NULL REFERENCES library_roots(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'disc_image', 'disc_folder', 'unrecognized_image')),
    source_path TEXT NOT NULL,
    dest_path TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('hardlink_or_copy', 'copy', 'move')),
    size_bytes INTEGER NOT NULL,
    media_file_id INTEGER REFERENCES media_files(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
