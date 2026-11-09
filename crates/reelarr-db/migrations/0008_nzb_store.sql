-- Durable NZB execution data for the built-in downloader (#76). Clean-room.
-- The worker stores the NZB *before* inserting its job and removes it again if
-- that insert fails (reelarr-nzb admission order), so nzb_documents.job_id
-- cannot carry a FOREIGN KEY to jobs. Instead a job delete cascades through the
-- trigger below, and SqliteNzbStore::open removes rows with no job (an admission
-- that crashed between the two inserts).
-- NOTE: a future migration that rebuilds `jobs` (CREATE jobs_new / DROP / RENAME,
-- as 0005 does) drops this trigger and must recreate it.
CREATE TABLE nzb_documents (
    job_id TEXT PRIMARY KEY NOT NULL,
    -- Not a FOREIGN KEY: the importer reports unknown movies itself, and a
    -- removed movie must not block the download record.
    movie_id INTEGER,
    -- Versioned JSON of the immutable parsed document (may hold an archive password).
    document TEXT NOT NULL,
    -- JSON array: number of segments per file, used to validate checkpoints
    -- without decoding the whole document.
    segment_counts TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- One row per segment whose verified bytes are durably on disk.
CREATE TABLE nzb_checkpoints (
    job_id TEXT NOT NULL REFERENCES nzb_documents(job_id) ON DELETE CASCADE,
    file_index INTEGER NOT NULL CHECK (file_index >= 0),
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 4294967295),
    PRIMARY KEY (job_id, file_index, number)
) WITHOUT ROWID;

CREATE TRIGGER nzb_documents_job_deleted AFTER DELETE ON jobs
BEGIN
    DELETE FROM nzb_documents WHERE job_id = OLD.id;
END;
