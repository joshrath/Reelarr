use reelarr_db::Database;
use reelarr_stream::{Lookup, LookupError, LookupFuture, MediaLookup, PlayableFile};
use reelarr_types::MediaFileId;
use sqlx::Row;

use super::files::probe_from_json;

/// [`MediaLookup`] for playback, reading `media_files` joined to its root.
///
/// Missing files are [`Lookup::NotFound`]. Files under a root whose last
/// health check was offline or permission denied are
/// [`Lookup::Unavailable`], so the player can say the share is down rather
/// than that the movie is gone.
#[derive(Debug, Clone)]
pub struct DbMediaLookup {
    db: Database,
}

impl DbMediaLookup {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    async fn find(&self, id: MediaFileId) -> Result<Lookup, LookupError> {
        let err = |e: sqlx::Error| LookupError(e.to_string());
        let Some(row) = sqlx::query(
            "SELECT f.path, f.missing_since, f.probe, r.path AS root_path, r.health, \
             r.health_reason FROM media_files f JOIN library_roots r ON r.id = f.root_id \
             WHERE f.id = ?",
        )
        .bind(id.0)
        .fetch_optional(self.db.pool())
        .await
        .map_err(err)?
        else {
            return Ok(Lookup::NotFound);
        };
        let missing: Option<i64> = row.try_get("missing_since").map_err(err)?;
        if missing.is_some() {
            return Ok(Lookup::NotFound);
        }
        let health: String = row.try_get("health").map_err(err)?;
        if health == "offline" || health == "permission_denied" {
            let reason: Option<String> = row.try_get("health_reason").map_err(err)?;
            return Ok(Lookup::Unavailable {
                reason: reason.unwrap_or_else(|| "library folder is offline".into()),
            });
        }
        let probe = probe_from_json(row.try_get("probe").map_err(err)?)
            .map_err(|e| LookupError(e.to_string()))?;
        Ok(Lookup::Found(PlayableFile {
            id,
            path: row.try_get::<String, _>("path").map_err(err)?.into(),
            root: row.try_get::<String, _>("root_path").map_err(err)?.into(),
            probe,
        }))
    }
}

impl MediaLookup for DbMediaLookup {
    fn lookup(&self, id: MediaFileId) -> LookupFuture<'_> {
        Box::pin(self.find(id))
    }
}
