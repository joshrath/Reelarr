use reelarr_core::{
    CompletedStage, Failure, FailureCode, Job, JobKind, JobState, JobStoreError, Priority, Progress,
};
use sqlx::{Row, Sqlite, SqliteConnection, sqlite::SqliteRow};

pub(super) fn backend(error: impl std::error::Error + Send + Sync + 'static) -> JobStoreError {
    JobStoreError::Backend(Box::new(error))
}

pub(super) fn invalid(message: &str) -> JobStoreError {
    backend(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

macro_rules! enum_codec {
    ($ty:ident, $encode:ident, $decode:ident, {$($variant:ident => $value:literal),+ $(,)?}) => {
        pub(super) fn $encode(value: $ty) -> &'static str {
            match value { $($ty::$variant => $value),+ }
        }
        fn $decode(value: &str) -> Result<$ty, JobStoreError> {
            match value { $($value => Ok($ty::$variant)),+, _ => Err(invalid(concat!("invalid persisted ", stringify!($ty)))) }
        }
    }
}

enum_codec!(JobKind, kind, parse_kind, {Nzb => "nzb", ExternalDownload => "external_download"});
enum_codec!(JobState, state, parse_state, {
    Queued => "queued", Downloading => "downloading", Paused => "paused",
    Verifying => "verifying", Repairing => "repairing", Extracting => "extracting",
    ReadyForImport => "ready_for_import", Importing => "importing", Completed => "completed",
    Failed => "failed", Cancelled => "cancelled"
});
enum_codec!(FailureCode, code, parse_code, {
    MissingArticles => "missing_articles", RepairFailed => "repair_failed", ExtractFailed => "extract_failed",
    PasswordRequired => "password_required", DiskFull => "disk_full", ServerAuth => "server_auth",
    InvalidNzb => "invalid_nzb", Duplicate => "duplicate", NoMatch => "no_match",
    PathUnmapped => "path_unmapped", Other => "other"
});
enum_codec!(CompletedStage, hint, parse_hint, {
    Downloading => "downloading", Verifying => "verifying", Repairing => "repairing", Extracting => "extracting"
});

pub(super) struct Snapshot {
    pub job: Job,
    pub durable_seq: u64,
    pub reserved: u64,
}

fn col<T>(row: &SqliteRow, name: &str) -> Result<T, JobStoreError>
where
    T: for<'a> sqlx::Decode<'a, Sqlite> + sqlx::Type<Sqlite>,
{
    row.try_get(name).map_err(backend)
}

pub(super) fn uint(row: &SqliteRow, name: &str) -> Result<u64, JobStoreError> {
    let bytes: Vec<u8> = col(row, name)?;
    Ok(u64::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| invalid("invalid unsigned integer width"))?,
    ))
}

pub(super) fn decode(row: SqliteRow) -> Result<Snapshot, JobStoreError> {
    let reserved = uint(&row, "seq_reserved")?;
    let failure = col::<Option<String>>(&row, "failure_code")?
        .map(|value| {
            Ok::<_, JobStoreError>(Failure {
                code: parse_code(&value)?,
                message: col(&row, "failure_message")?,
                retryable: col(&row, "failure_retryable")?,
            })
        })
        .transpose()?;
    let priority = match col::<i64>(&row, "priority")? {
        0 => Priority::Force,
        1 => Priority::High,
        2 => Priority::Normal,
        3 => Priority::Low,
        _ => return Err(invalid("invalid persisted priority")),
    };
    Ok(Snapshot {
        durable_seq: uint(&row, "event_seq")?,
        reserved,
        job: Job {
            id: col::<String>(&row, "id")?.parse().map_err(backend)?,
            kind: parse_kind(&col::<String>(&row, "kind")?)?,
            state: parse_state(&col::<String>(&row, "state")?)?,
            title: col(&row, "title")?,
            priority,
            queue_position: uint(&row, "queue_position")?,
            category: col(&row, "category")?,
            failure,
            attempts: u32::try_from(col::<i64>(&row, "attempts")?).map_err(backend)?,
            created_at: col(&row, "created_at")?,
            updated_at: col(&row, "updated_at")?,
            resume_state: col::<Option<String>>(&row, "resume_state")?
                .as_deref()
                .map(parse_state)
                .transpose()?,
            resume_hint: col::<Option<String>>(&row, "resume_hint")?
                .as_deref()
                .map(parse_hint)
                .transpose()?,
            // After restart skip the entire previous lease, never reuse a possibly sent seq.
            last_event_seq: reserved,
            progress: Progress::default(),
        },
    })
}

pub(super) async fn save(
    conn: &mut SqliteConnection,
    job: &Job,
    reserved: u64,
    insert: bool,
) -> Result<(), JobStoreError> {
    let sql = if insert {
        "INSERT INTO jobs (id, kind, state, title, priority, queue_position, category,
         failure_code, failure_message, failure_retryable, attempts, resume_state,
         resume_hint, created_at, updated_at, event_seq, seq_reserved)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
    } else {
        "UPDATE jobs SET kind=?2, state=?3, title=?4, priority=?5, queue_position=?6,
         category=?7, failure_code=?8, failure_message=?9, failure_retryable=?10,
         attempts=?11, resume_state=?12, resume_hint=?13, created_at=?14,
         updated_at=?15, event_seq=?16, seq_reserved=?17 WHERE id=?1"
    };
    let priority: i64 = match job.priority {
        Priority::Force => 0,
        Priority::High => 1,
        Priority::Normal => 2,
        Priority::Low => 3,
    };
    sqlx::query(sql)
        .bind(job.id.to_string())
        .bind(kind(job.kind))
        .bind(state(job.state))
        .bind(&job.title)
        .bind(priority)
        .bind(job.queue_position.to_be_bytes().to_vec())
        .bind(job.category.as_deref())
        .bind(job.failure.as_ref().map(|f| code(f.code)))
        .bind(job.failure.as_ref().map(|f| f.message.as_str()))
        .bind(job.failure.as_ref().map(|f| f.retryable))
        .bind(i64::from(job.attempts))
        .bind(job.resume_state.map(state))
        .bind(job.resume_hint.map(hint))
        .bind(job.created_at)
        .bind(job.updated_at)
        .bind(job.last_event_seq.to_be_bytes().to_vec())
        .bind(reserved.to_be_bytes().to_vec())
        .execute(conn)
        .await
        .map_err(backend)?;
    Ok(())
}
