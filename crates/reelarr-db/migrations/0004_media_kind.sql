-- Disc rips (#67): each ISO or VIDEO_TS/BDMV folder is one media_files row.
-- Existing rows are regular video files.
ALTER TABLE media_files ADD COLUMN kind TEXT NOT NULL DEFAULT 'file'
    CHECK (kind IN ('file', 'disc_image', 'disc_folder', 'unrecognized_image'));
