-- Default import folder (#100). Forward-only.
-- At most one root is the default for new-movie imports (the partial unique
-- index), and only a 'library' root can be it (the CHECK). Existing roots
-- start with no default: a single library root is still used implicitly, and
-- with several the user is asked which one is the main library.
ALTER TABLE library_roots
    ADD COLUMN is_default INTEGER NOT NULL DEFAULT 0
        CHECK (is_default IN (0, 1) AND (is_default = 0 OR kind = 'library'));

CREATE UNIQUE INDEX library_roots_one_default_idx
    ON library_roots(is_default) WHERE is_default = 1;
