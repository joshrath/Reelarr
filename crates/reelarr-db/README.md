# Database foundation

`Database::connect(path)` creates the SQLite file if needed, configures every pool
connection with WAL, foreign keys and a five-second busy timeout, then runs the
embedded forward-only migrations. The parent directory must already exist. A
migration failure closes the pool and returns an error; do not serve requests
until connection succeeds. Clones of `Database` share the same pool.

The server creates `$REELARR_DATA_DIR` (default `/config`) and opens `reelarr.db`
before binding its listener. `/health` remains a liveness endpoint at the root;
feature API routes attach to the stateful router nested at `/api/v1`.

`reelarr-db` owns the single database and **all** migrations. Add numbered `.sql`
files under `migrations/`; never change an already applied migration. `build.rs`
ensures newly added migration files are picked up by incremental builds. No live
DB or query metadata is needed to compile: queries use runtime `sqlx::query`.

`SettingsStore` in `reelarr-core` is async and object-safe, with an in-memory
implementation for domain tests. `SqliteSettingsStore::new(&db)` shares the pool.
Values are opaque strings, missing keys yield `None`, and `set` atomically
upserts the value and its Unix timestamp in whole seconds (`updated_at`).
Job tables and lifecycle persistence are intentionally deferred to #29.
