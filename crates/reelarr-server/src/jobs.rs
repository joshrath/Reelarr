//! Job API (#76 PR 5).
//!
//! - `GET    /jobs`                 all jobs in scheduling order ([`JobList`])
//! - `GET    /jobs/{id}`            one job ([`JobSnapshot`])
//! - `POST   /jobs/upload`          raw NZB body (at most [`MAX_NZB_UPLOAD`]),
//!   options in the query string ([`UploadNzbParams`]); 201 with the job
//! - `POST   /jobs/release`         fetch a release's NZB through the named
//!   indexer's shared client ([`EnqueueReleaseRequest`]); 201 with the job
//! - `POST   /jobs/{id}/pause`      [`Supervisor::pause`]
//! - `POST   /jobs/{id}/resume`     [`Supervisor::resume`]
//! - `POST   /jobs/{id}/cancel`     [`Supervisor::cancel`], then
//!   [`Supervisor::discard`] for the staging files
//! - `PUT    /jobs/{id}/priority`   [`SetPriorityRequest`]; keeps the position
//! - `PUT    /jobs/order`           [`ReorderRequest`], one transaction
//! - `GET    /jobs/events`          Server-Sent Events, see below
//!
//! Every route is in the protected router (session required, 401 otherwise);
//! POST/PUT also need the CSRF double-submit proof (403). Both enqueue paths
//! go through [`Supervisor::enqueue_nzb`], which wakes the scheduler.
//! Responses never include the NZB document, its archive passwords, the
//! release download URL, indexer API keys or NNTP credentials; error
//! messages are static.
//!
//! **Event stream.** `GET /jobs/events` subscribes to the job event hub and
//! then sends an `event: snapshot` frame ([`JobList`]). After it, each
//! `event: job` frame is a [`reelarr_types::JobEvent`] with the job's
//! per-job `seq`. Events at or below a job's snapshot `last_event_seq` are
//! dropped by the server, so the snapshot plus the following events are
//! consistent. The hub is a bounded, lossy broadcast: if this stream falls
//! behind, the server sends a fresh `snapshot` frame and continues from it.
//! Clients replace their whole job state on every `snapshot`. There is no
//! durable replay: frames carry no SSE `id`, `Last-Event-ID` is ignored, and
//! a reconnect always starts with a new snapshot. Missing seq numbers within
//! a job are normal (progress is throttled and sequence leases skip ahead
//! after a restart); only the order matters.
use std::{collections::HashMap, convert::Infallible};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{BytesRejection, JsonRejection, QueryRejection},
    },
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post, put},
};
use futures_util::{Stream, stream};
use reelarr_core::{Job, JobId, JobStore, JobStoreError, Priority};
use reelarr_db::ReorderError;
use reelarr_indexer::{IndexerError, Release, SecretString};
use reelarr_nzb::worker::EnqueueError;
use reelarr_types::{
    ApiError, EnqueueReleaseRequest, JobList, JobSnapshot, MovieId, ReorderRequest,
    SetPriorityRequest, UploadNzbParams,
};
use tokio::sync::broadcast::{Receiver, error::RecvError};

use crate::{
    downloads::{AdmitError, Discarded, NewDownload, Supervisor},
    state::AppState,
};

/// Largest accepted NZB upload: the NZB parser's own input ceiling (32 MiB).
pub const MAX_NZB_UPLOAD: usize = 32 * 1024 * 1024;
/// Longest accepted title, category or indexer name, in bytes.
const MAX_TEXT: usize = 1024;
/// Longest accepted release download URL, in bytes.
const MAX_URL: usize = 8 * 1024;
/// Most jobs one reorder request may list.
const MAX_REORDER: usize = 10_000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/jobs", get(list))
        .route("/jobs/events", get(events))
        .route("/jobs/order", put(reorder))
        .route(
            "/jobs/upload",
            post(upload).layer(DefaultBodyLimit::max(MAX_NZB_UPLOAD)),
        )
        .route("/jobs/release", post(release))
        .route("/jobs/{id}", get(read))
        .route("/jobs/{id}/pause", post(pause))
        .route("/jobs/{id}/resume", post(resume))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/jobs/{id}/priority", put(priority))
}

pub struct JobsApiError(StatusCode, ApiError);
impl IntoResponse for JobsApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}
fn error(status: StatusCode, code: &str, message: impl Into<String>) -> JobsApiError {
    JobsApiError(
        status,
        ApiError {
            code: code.into(),
            message: message.into(),
        },
    )
}
fn internal(what: &str) -> JobsApiError {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal", what)
}
impl From<JobStoreError> for JobsApiError {
    fn from(e: JobStoreError) -> Self {
        match e {
            JobStoreError::NotFound(_) => {
                error(StatusCode::NOT_FOUND, "job_not_found", "No such job")
            }
            JobStoreError::Transition(_) => error(
                StatusCode::CONFLICT,
                "invalid_transition",
                "The job is not in a state that allows this",
            ),
            other => {
                tracing::error!(error = %other, "job storage failed");
                internal("The job store could not be read or updated")
            }
        }
    }
}
impl From<AdmitError> for JobsApiError {
    fn from(e: AdmitError) -> Self {
        match e {
            // Parser messages name the limit or construct, never input values.
            AdmitError::Enqueue(EnqueueError::InvalidNzb(e)) => error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_nzb",
                format!("The NZB could not be read: {e}"),
            ),
            AdmitError::Enqueue(EnqueueError::Worker(e)) => {
                tracing::error!(error = %e, "NZB admission failed");
                internal("The NZB could not be stored")
            }
            AdmitError::Jobs(e) => e.into(),
            AdmitError::Join => internal("The NZB could not be stored"),
        }
    }
}
impl From<ReorderError> for JobsApiError {
    fn from(e: ReorderError) -> Self {
        match e {
            ReorderError::Duplicate(_) => error(
                StatusCode::BAD_REQUEST,
                "duplicate_job",
                "A job is listed more than once",
            ),
            ReorderError::NotFound(_) => {
                error(StatusCode::NOT_FOUND, "job_not_found", "No such job")
            }
            ReorderError::Finished(_) => error(
                StatusCode::CONFLICT,
                "job_finished",
                "Finished jobs cannot be reordered",
            ),
            ReorderError::Store(e) => e.into(),
        }
    }
}
fn indexer_error(e: IndexerError) -> JobsApiError {
    use IndexerError as E;
    match e {
        E::UnsafeDownloadUrl => error(
            StatusCode::BAD_REQUEST,
            "download_url_not_allowed",
            "The download URL is not on this indexer or its allowed download hosts",
        ),
        E::TooManyRedirects => error(
            StatusCode::BAD_GATEWAY,
            "indexer_error",
            "The indexer redirected the download too many times",
        ),
        E::Disabled => error(
            StatusCode::CONFLICT,
            "indexer_disabled",
            "This indexer is disabled",
        ),
        E::RateLimited { retry_after } => error(
            StatusCode::TOO_MANY_REQUESTS,
            "indexer_rate_limited",
            format!(
                "The indexer is rate limiting requests; retry in {} s",
                retry_after.as_secs().max(1)
            ),
        ),
        E::Timeout => error(
            StatusCode::GATEWAY_TIMEOUT,
            "indexer_timeout",
            "The indexer did not answer in time",
        ),
        E::ResponseTooLarge => error(
            StatusCode::BAD_GATEWAY,
            "indexer_error",
            "The indexer's NZB is larger than allowed",
        ),
        E::InvalidNzb => error(
            StatusCode::BAD_GATEWAY,
            "indexer_error",
            "The indexer did not return an NZB",
        ),
        E::BadApiKey | E::Unauthorized | E::AccountSuspended => error(
            StatusCode::BAD_GATEWAY,
            "indexer_auth",
            "The indexer rejected Reelarr's API key",
        ),
        E::InvalidConfig => error(
            StatusCode::CONFLICT,
            "indexer_misconfigured",
            "This indexer's stored settings are invalid",
        ),
        E::Store => {
            tracing::error!("indexer settings unreadable");
            internal("The indexer settings could not be read")
        }
        other => {
            tracing::info!(error = ?other, "NZB fetch failed");
            error(
                StatusCode::BAD_GATEWAY,
                "indexer_error",
                "The indexer could not provide the NZB",
            )
        }
    }
}

fn job_id(raw: &str) -> Result<JobId, JobsApiError> {
    raw.parse().map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_job_id",
            "Job ids are UUIDv7 strings",
        )
    })
}
fn snapshot(job: Job) -> Json<JobSnapshot> {
    Json(job.into())
}
fn created(job: Job) -> Response {
    (StatusCode::CREATED, snapshot(job)).into_response()
}
/// Optional single-line text field: trimmed, non-empty, bounded.
fn text(value: Option<String>, field: &str) -> Result<Option<String>, JobsApiError> {
    let Some(value) = value else { return Ok(None) };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_TEXT || value.chars().any(char::is_control) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_field",
            format!("{field} must be at most {MAX_TEXT} bytes with no control characters"),
        ));
    }
    Ok(Some(value.to_owned()))
}
fn required(value: String, field: &str) -> Result<String, JobsApiError> {
    text(Some(value), field)?.ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_field",
            format!("{field} is required"),
        )
    })
}
fn movie(id: Option<i64>) -> Result<Option<MovieId>, JobsApiError> {
    match id {
        Some(id) if id <= 0 => Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_field",
            "movie_id must be positive",
        )),
        other => Ok(other.map(MovieId)),
    }
}

async fn list(State(state): State<AppState>) -> Result<Json<JobList>, JobsApiError> {
    Ok(Json(job_list(state.jobs.list().await?)))
}
fn job_list(jobs: Vec<Job>) -> JobList {
    JobList {
        jobs: jobs.into_iter().map(Into::into).collect(),
    }
}

async fn read(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<JobSnapshot>, JobsApiError> {
    Ok(snapshot(state.jobs.get(job_id(&id)?).await?))
}

async fn upload(
    State(state): State<AppState>,
    params: Result<Query<UploadNzbParams>, QueryRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, JobsApiError> {
    let Query(params) = params.map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "Expected title, category, movie_id and priority query parameters",
        )
    })?;
    let body = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "nzb_too_large",
                format!(
                    "NZB uploads are limited to {} MiB",
                    MAX_NZB_UPLOAD / 1024 / 1024
                ),
            )
        } else {
            error(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                "The upload body could not be read",
            )
        }
    })?;
    if body.is_empty() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "empty_nzb",
            "Send the NZB file as the request body",
        ));
    }
    let new = NewDownload {
        fallback_title: text(params.title, "title")?.unwrap_or_else(|| "Uploaded NZB".into()),
        category: text(params.category, "category")?,
        movie_id: movie(params.movie_id)?,
        priority: params.priority.map(Into::into).unwrap_or_default(),
    };
    let job = state.downloads.enqueue_nzb(body.to_vec(), new).await?;
    tracing::info!(job = %job.id, "queued uploaded NZB");
    Ok(created(job))
}

async fn release(
    State(state): State<AppState>,
    body: Result<Json<EnqueueReleaseRequest>, JsonRejection>,
) -> Result<Response, JobsApiError> {
    // Rejection text can quote the body, which holds the download URL.
    let Json(request) = body.map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "Expected indexer, download_url and title",
        )
    })?;
    let indexer = required(request.indexer, "indexer")?;
    let title = required(request.title, "title")?;
    let new = NewDownload {
        fallback_title: title.clone(),
        category: text(request.category, "category")?,
        movie_id: movie(request.movie_id)?,
        priority: request.priority.map(Into::into).unwrap_or_default(),
    };
    let url = request.download_url.trim();
    if url.is_empty() || url.len() > MAX_URL {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_field",
            format!("download_url is required and at most {MAX_URL} bytes"),
        ));
    }
    let client = state
        .indexers
        .client(&indexer)
        .await
        .map_err(indexer_error)?
        .ok_or_else(|| {
            error(
                StatusCode::NOT_FOUND,
                "indexer_not_found",
                "No such indexer",
            )
        })?;
    let nzb = client
        .fetch_nzb(&release_for(title, url))
        .await
        .map_err(indexer_error)?;
    let job = state.downloads.enqueue_nzb(nzb, new).await?;
    tracing::info!(job = %job.id, "queued NZB from indexer");
    Ok(created(job))
}

fn release_for(title: String, url: &str) -> Release {
    Release {
        title,
        guid: String::new(),
        download_url: SecretString::new(url),
        size: None,
        pub_date: None,
        categories: Vec::new(),
        imdb_id: None,
        tmdb_id: None,
        grabs: None,
        files: None,
        password: None,
        year: None,
        attributes: Default::default(),
    }
}

async fn pause(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<JobSnapshot>, JobsApiError> {
    Ok(snapshot(state.downloads.pause(job_id(&id)?).await?))
}

async fn resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<JobSnapshot>, JobsApiError> {
    Ok(snapshot(state.downloads.resume(job_id(&id)?).await?))
}

/// Cancel through the supervisor, then discard the staging files. A failed
/// discard is logged; the cancel itself has already happened.
async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<JobSnapshot>, JobsApiError> {
    let id = job_id(&id)?;
    let job = state.downloads.cancel(id).await?;
    discard(&state.downloads, id).await;
    Ok(snapshot(job))
}
async fn discard(downloads: &Supervisor, id: JobId) {
    match downloads.discard(id).await {
        Ok(Discarded::Now) => {}
        Ok(Discarded::WhenStopped) => {
            tracing::debug!(job = %id, "staging files go when the run stops")
        }
        Err(error) => tracing::warn!(job = %id, %error, "could not discard staging files"),
    }
}

async fn priority(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<SetPriorityRequest>, JsonRejection>,
) -> Result<Json<JobSnapshot>, JobsApiError> {
    let id = job_id(&id)?;
    let Json(body) = body.map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "Expected {\"priority\": \"force\" | \"high\" | \"normal\" | \"low\"}",
        )
    })?;
    let current = state.jobs.get(id).await?;
    if current.state.is_terminal() {
        return Err(error(
            StatusCode::CONFLICT,
            "job_finished",
            "Finished jobs cannot be reprioritized",
        ));
    }
    let job = state
        .jobs
        .set_queue_order(id, Priority::from(body.priority), current.queue_position)
        .await?;
    state.downloads.wake();
    Ok(snapshot(job))
}

async fn reorder(
    State(state): State<AppState>,
    body: Result<Json<ReorderRequest>, JsonRejection>,
) -> Result<Json<JobList>, JobsApiError> {
    let Json(body) = body.map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "Expected {\"job_ids\": [...]}",
        )
    })?;
    if body.job_ids.is_empty() || body.job_ids.len() > MAX_REORDER {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_field",
            format!("job_ids must list 1 to {MAX_REORDER} jobs"),
        ));
    }
    let ids = body
        .job_ids
        .iter()
        .map(|id| job_id(id))
        .collect::<Result<Vec<_>, _>>()?;
    let jobs = state.jobs.reorder(&ids).await?;
    state.downloads.wake();
    Ok(Json(job_list(jobs)))
}

async fn events(
    State(state): State<AppState>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, JobsApiError> {
    // Subscribe before the snapshot, so nothing between them is lost.
    let receiver = state.jobs.events().subscribe();
    let jobs = state.jobs.list().await?;
    let feed = Feed {
        state,
        receiver,
        snapshot: Some(jobs),
        watermarks: HashMap::new(),
    };
    let stream = stream::unfold(feed, |mut feed| async move {
        let event = feed.next().await?;
        Some((Ok(event), feed))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

struct Feed {
    state: AppState,
    receiver: Receiver<reelarr_core::JobEvent>,
    /// A snapshot to send before any further event.
    snapshot: Option<Vec<Job>>,
    /// Per job: drop events at or below this seq (from the last snapshot).
    watermarks: HashMap<JobId, u64>,
}

impl Feed {
    async fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(jobs) = self.snapshot.take() {
                self.watermarks = jobs.iter().map(|j| (j.id, j.last_event_seq)).collect();
                return Some(json_event("snapshot", &job_list(jobs)));
            }
            match self.receiver.recv().await {
                Ok(event) => {
                    if self
                        .watermarks
                        .get(&event.job_id)
                        .is_some_and(|&seen| event.seq <= seen)
                    {
                        continue;
                    }
                    let wire = reelarr_types::JobEvent::from(event);
                    return Some(json_event("job", &wire));
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "job event stream lagged; resending snapshot");
                    match self.state.jobs.list().await {
                        Ok(jobs) => self.snapshot = Some(jobs),
                        Err(error) => {
                            // End the stream; the client reconnects and resyncs.
                            tracing::warn!(%error, "could not resync job event stream");
                            return None;
                        }
                    }
                }
                Err(RecvError::Closed) => return None,
            }
        }
    }
}

fn json_event(name: &str, value: &impl serde::Serialize) -> Event {
    Event::default()
        .event(name)
        .json_data(value)
        .unwrap_or_else(|_| Event::default().event(name).data("{}"))
}
