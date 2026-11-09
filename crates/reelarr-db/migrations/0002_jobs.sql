-- Forward-only job snapshots. Progress values live only in the process cache.
-- Unsigned u64 ordering/sequence values use fixed-width, big-endian BLOBs so
-- SQLite's signed INTEGER range cannot truncate or wrap valid domain values.
CREATE TABLE jobs (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('nzb', 'external_download')),
    state TEXT NOT NULL CHECK (state IN ('queued', 'downloading', 'paused',
        'verifying', 'repairing', 'extracting', 'ready_for_import', 'importing',
        'completed', 'failed', 'cancelled')),
    title TEXT NOT NULL,
    priority INTEGER NOT NULL CHECK (priority BETWEEN 0 AND 3),
    queue_position BLOB NOT NULL CHECK (length(queue_position) = 8),
    category TEXT,
    failure_code TEXT CHECK (failure_code IN ('missing_articles', 'repair_failed',
        'extract_failed', 'password_required', 'disk_full', 'server_auth',
        'invalid_nzb', 'duplicate', 'no_match', 'path_unmapped', 'other')),
    failure_message TEXT,
    failure_retryable INTEGER CHECK (failure_retryable IN (0, 1)),
    attempts INTEGER NOT NULL CHECK (attempts BETWEEN 0 AND 4294967295),
    resume_state TEXT CHECK (resume_state IN ('queued', 'downloading')),
    resume_hint TEXT CHECK (resume_hint IN ('downloading', 'verifying', 'repairing', 'extracting')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    event_seq BLOB NOT NULL CHECK (length(event_seq) = 8),
    seq_reserved BLOB NOT NULL CHECK (length(seq_reserved) = 8 AND seq_reserved >= event_seq),
    -- Registered local artifacts for startup recovery, not guessed from job titles.
    recovery_path TEXT,
    CHECK ((state = 'paused' AND resume_state IS NOT NULL) OR
           (state <> 'paused' AND resume_state IS NULL)),
    CHECK ((state = 'failed' AND failure_code IS NOT NULL AND
            failure_message IS NOT NULL AND failure_retryable IS NOT NULL) OR
           (state <> 'failed' AND failure_code IS NULL AND
            failure_message IS NULL AND failure_retryable IS NULL))
);
CREATE INDEX jobs_state_idx ON jobs(state);
CREATE INDEX jobs_queue_idx ON jobs(priority, queue_position);
