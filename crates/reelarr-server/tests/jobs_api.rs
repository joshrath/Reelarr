//! Job API (#76 PR 5): auth/CSRF, validation, upload bounds, enqueue via
//! upload and indexer, supervisor pause/resume/cancel with discard, priority
//! and atomic reorder, and the SSE snapshot/seq contract.
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use reelarr_core::{EventHub, JobId, JobState, JobStore};
use reelarr_db::{
    Database, NntpServer, SqliteIndexerStore, SqliteJobStore,
    secrets::{SecretCipher, XChaChaSecretCipher},
};
use reelarr_indexer::{IndexerConfig, IndexerStore, SecretString, Url};
use reelarr_nzb::worker::{CancelToken, Outcome, WorkerError};
use reelarr_server::{
    app,
    downloads::{BuildError, JobRunner, RunnerFactory, Supervisor, SupervisorConfig},
    jobs::MAX_NZB_UPLOAD,
    state::{AppState, DownloadOptions},
};
use reelarr_types::{
    ApiError, JobEvent, JobEventKind, JobList, JobSnapshot, Priority, SessionInfo,
    nntp::{NntpServerUpdate, PasswordUpdate},
};
use tower::ServiceExt;

const ARCHIVE_PASSWORD: &str = "archive-password-never-returned-71c2";
const NNTP_PASSWORD: &str = "nntp-password-never-returned-9a0e";
const API_KEY: &str = "indexer-api-key-never-returned-4d1b";

fn nzb(title: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\
         <head><meta type=\"title\">{title}</meta><meta type=\"password\">{ARCHIVE_PASSWORD}</meta></head>\
         <file poster=\"p@example.test\" date=\"1700000000\" subject=\"&quot;a.mkv&quot; yEnc (1/1)\">\
         <groups><group>alt.binaries.test</group></groups>\
         <segments><segment bytes=\"10\" number=\"1\">{title}-seg@example.test</segment></segments></file></nzb>"
    )
}

/// Scripted runner: marks the job Downloading, leaves a staging file, then
/// waits for its cancel token. Records whether the staging files were still
/// there when the run returned (discard must wait for the run).
#[derive(Default)]
struct Script {
    starts: Mutex<Vec<JobId>>,
    stops: Mutex<Vec<(JobId, bool)>>,
}
struct Factory(Arc<Script>, Arc<SqliteJobStore>, PathBuf);
#[async_trait]
impl RunnerFactory for Factory {
    async fn build(&self, _: NntpServer) -> Result<Arc<dyn JobRunner>, BuildError> {
        Ok(Arc::new(Runner(
            self.0.clone(),
            self.1.clone(),
            self.2.clone(),
        )))
    }
}
struct Runner(Arc<Script>, Arc<SqliteJobStore>, PathBuf);
#[async_trait]
impl JobRunner for Runner {
    async fn run(&self, id: JobId, cancel: &CancelToken) -> Result<Outcome, WorkerError> {
        if self.1.get(id).await?.state == JobState::Queued
            && self
                .1
                .transition(id, JobState::Downloading, None)
                .await
                .is_err()
        {
            return Ok(Outcome::Stopped);
        }
        let dir = self.2.join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("part.bin"), b"partial").unwrap();
        self.0.starts.lock().unwrap().push(id);
        cancel.cancelled().await;
        // Still stopping: a discard now would pull files from under us.
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.0.stops.lock().unwrap().push((id, dir.exists()));
        Ok(Outcome::Stopped)
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    staging: PathBuf,
    state: AppState,
    router: Router,
    script: Arc<Script>,
    cipher: Arc<dyn SecretCipher>,
    cookie: String,
    csrf: String,
}

async fn env(hub_capacity: usize) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::connect(tmp.path().join("reelarr.db"))
        .await
        .unwrap();
    // The first job store for a database picks the shared event hub.
    SqliteJobStore::with_options(&db, EventHub::new(hub_capacity), Duration::ZERO);
    let cipher: Arc<dyn SecretCipher> = Arc::new(XChaChaSecretCipher::ephemeral().unwrap());
    let options = DownloadOptions::new(tmp.path().join("downloads"));
    let staging = options.worker_config().staging_root;
    let mut state = AppState::with_options(db, cipher.clone(), options.clone());
    let script = Arc::new(Script::default());
    // Same store and folders, scripted runs; the poll interval is long so
    // only wake() can dispatch new work during a test.
    state.downloads = Supervisor::new(
        state.jobs.clone(),
        reelarr_db::SqliteNzbStore::new(&state.db),
        state.nntp.clone(),
        Arc::new(Factory(script.clone(), state.jobs.clone(), staging.clone())),
        options.worker_config(),
        SupervisorConfig {
            max_concurrent_jobs: 2,
            poll_interval: Duration::from_secs(3600),
            ..SupervisorConfig::default()
        },
    );
    state.downloads.start().await.unwrap();
    let router = app(state.clone());
    let (cookie, csrf) = sign_in(&router).await;
    Env {
        _tmp: tmp,
        staging,
        state,
        router,
        script,
        cipher,
        cookie,
        csrf,
    }
}

impl Env {
    async fn configure_nntp(&self) {
        let mut update = NntpServerUpdate::new("news.example.test");
        update.username = Some("user".into());
        update.password = PasswordUpdate::Set(NNTP_PASSWORD.into());
        self.state.nntp.save(&update).await.unwrap();
        self.state.downloads.nntp_settings_changed();
        wait("NNTP generation", || async {
            self.state.downloads.current_generation().is_some()
        })
        .await;
    }
    async fn call(&self, method: &str, uri: &str, body: impl Into<Body>) -> Response {
        call(
            &self.router,
            method,
            uri,
            &self.cookie,
            Some(&self.csrf),
            body.into(),
        )
        .await
    }
    async fn upload(&self, title: &str) -> JobSnapshot {
        let resp = self
            .call("POST", "/api/v1/jobs/upload?title=fallback", nzb(title))
            .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        json(resp).await
    }
    async fn get(&self, id: &str) -> JobSnapshot {
        json(self.call("GET", &format!("/api/v1/jobs/{id}"), "").await).await
    }
    async fn list(&self) -> Vec<JobSnapshot> {
        json::<JobList>(self.call("GET", "/api/v1/jobs", "").await)
            .await
            .jobs
    }
    fn started(&self, id: &str) -> usize {
        self.script
            .starts
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.to_string() == id)
            .count()
    }
}

async fn wait<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..300 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: &str,
    csrf: Option<&str>,
    body: Body,
) -> Response {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = csrf {
        req = req.header("x-csrf-token", token);
    }
    app.clone().oneshot(req.body(body).unwrap()).await.unwrap()
}
async fn text(resp: Response) -> String {
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}
async fn json<T: serde::de::DeserializeOwned>(resp: Response) -> T {
    let status = resp.status();
    let body = text(resp).await;
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{status} {body}: {e}"))
}
async fn api_error(resp: Response, status: StatusCode) -> String {
    assert_eq!(resp.status(), status);
    json::<ApiError>(resp).await.code
}
fn cookies(resp: &Response) -> String {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|h| h.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("; ")
}
async fn sign_in(app: &Router) -> (String, String) {
    let resp = call(app, "GET", "/api/v1/auth/session", "", None, Body::empty()).await;
    let guest = cookies(&resp);
    let info: SessionInfo = json(resp).await;
    let resp = call(
        app,
        "POST",
        "/api/v1/auth/setup",
        &guest,
        Some(&info.csrf_token),
        r#"{"username":"admin","password":"a-long-password"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = cookies(&resp);
    let info: SessionInfo = json(resp).await;
    (cookie, info.csrf_token)
}

fn assert_secret_free(body: &str) {
    for secret in [
        ARCHIVE_PASSWORD,
        NNTP_PASSWORD,
        API_KEY,
        "-seg@example.test",
    ] {
        assert!(!body.contains(secret), "response leaked {secret}: {body}");
    }
}

#[tokio::test]
async fn every_route_needs_a_session_and_mutations_need_csrf() {
    let env = env(256).await;
    let id = env.upload("Auth").await.id;
    let routes = [
        ("GET", "/api/v1/jobs".to_owned()),
        ("GET", format!("/api/v1/jobs/{id}")),
        ("GET", "/api/v1/jobs/events".to_owned()),
        ("POST", "/api/v1/jobs/upload".to_owned()),
        ("POST", "/api/v1/jobs/release".to_owned()),
        ("POST", format!("/api/v1/jobs/{id}/pause")),
        ("POST", format!("/api/v1/jobs/{id}/resume")),
        ("POST", format!("/api/v1/jobs/{id}/cancel")),
        ("PUT", format!("/api/v1/jobs/{id}/priority")),
        ("PUT", "/api/v1/jobs/order".to_owned()),
    ];
    for (method, uri) in &routes {
        let resp = call(&env.router, method, uri, "", None, nzb("x").into()).await;
        assert_eq!(
            api_error(resp, StatusCode::UNAUTHORIZED).await,
            "unauthorized",
            "{uri}"
        );
        if *method != "GET" {
            let resp = call(&env.router, method, uri, &env.cookie, None, nzb("x").into()).await;
            assert_eq!(
                api_error(resp, StatusCode::FORBIDDEN).await,
                "csrf_rejected",
                "{uri}"
            );
        }
    }
    // Nothing above changed the job or queued another.
    let jobs = env.list().await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, reelarr_types::JobState::Queued);
}

#[tokio::test]
async fn upload_is_bounded_validated_and_never_echoes_the_nzb() {
    let env = env(256).await;
    let too_big = vec![b' '; MAX_NZB_UPLOAD + 1];
    let resp = env.call("POST", "/api/v1/jobs/upload", too_big).await;
    assert_eq!(
        api_error(resp, StatusCode::PAYLOAD_TOO_LARGE).await,
        "nzb_too_large"
    );
    let resp = env.call("POST", "/api/v1/jobs/upload", "").await;
    assert_eq!(api_error(resp, StatusCode::BAD_REQUEST).await, "empty_nzb");
    let resp = env.call("POST", "/api/v1/jobs/upload", "<nzb>").await;
    assert_eq!(
        api_error(resp, StatusCode::UNPROCESSABLE_ENTITY).await,
        "invalid_nzb"
    );
    for query in ["movie_id=0", "title=a%07b", "priority=urgent", "movie_id=x"] {
        let resp = env
            .call("POST", &format!("/api/v1/jobs/upload?{query}"), nzb("v"))
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{query}");
    }
    assert!(env.list().await.is_empty(), "rejected uploads created jobs");

    let resp = env
        .call(
            "POST",
            "/api/v1/jobs/upload?category=movies&movie_id=7&priority=high",
            nzb("Movie.2024.1080p"),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = text(resp).await;
    assert_secret_free(&body);
    let job: JobSnapshot = serde_json::from_str(&body).unwrap();
    assert_eq!(job.title, "Movie.2024.1080p");
    assert_eq!(job.category.as_deref(), Some("movies"));
    assert_eq!(job.priority, Priority::High);
    assert_eq!(job.state, reelarr_types::JobState::Queued);
    let stored = reelarr_db::SqliteNzbStore::new(&env.state.db);
    let stored = reelarr_nzb::NzbStore::load(&stored, job.id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.movie_id, Some(reelarr_types::MovieId(7)));

    env.configure_nntp().await;
    for uri in [
        "/api/v1/jobs".to_owned(),
        format!("/api/v1/jobs/{}", job.id),
    ] {
        let resp = env.call("GET", &uri, "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_secret_free(&text(resp).await);
    }
    let resp = env.call("GET", "/api/v1/jobs/not-a-uuid", "").await;
    assert_eq!(
        api_error(resp, StatusCode::BAD_REQUEST).await,
        "invalid_job_id"
    );
    let resp = env
        .call("GET", &format!("/api/v1/jobs/{}", JobId::new()), "")
        .await;
    assert_eq!(
        api_error(resp, StatusCode::NOT_FOUND).await,
        "job_not_found"
    );
}

#[tokio::test]
async fn enqueue_wakes_the_supervisor() {
    let env = env(256).await;
    env.configure_nntp().await;
    // The rescan interval is an hour: only wake() can start this job.
    let job = env.upload("Wake").await;
    wait("upload to start", || async { env.started(&job.id) == 1 }).await;
}

#[tokio::test]
async fn pause_resume_and_cancel_go_through_the_supervisor() {
    let env = env(256).await;
    env.configure_nntp().await;
    let job = env.upload("Control").await;
    let id = job.id.clone();
    let core_id: JobId = id.parse().unwrap();
    wait("start", || async { env.started(&id) == 1 }).await;

    let resp = env
        .call("POST", &format!("/api/v1/jobs/{id}/pause"), "")
        .await;
    let paused: JobSnapshot = json(resp).await;
    assert_eq!(paused.state, reelarr_types::JobState::Paused);
    assert_eq!(
        paused.resume_state,
        Some(reelarr_types::JobState::Downloading)
    );
    // State first, then the token: the run stops without touching state.
    wait("run to stop", || async {
        env.state.downloads.active().is_empty()
    })
    .await;
    assert_eq!(env.get(&id).await.state, reelarr_types::JobState::Paused);
    let resp = env
        .call("POST", &format!("/api/v1/jobs/{id}/pause"), "")
        .await;
    assert_eq!(
        api_error(resp, StatusCode::CONFLICT).await,
        "invalid_transition"
    );

    let resp = env
        .call("POST", &format!("/api/v1/jobs/{id}/resume"), "")
        .await;
    assert_eq!(
        json::<JobSnapshot>(resp).await.state,
        reelarr_types::JobState::Downloading
    );
    wait("restart", || async { env.started(&id) == 2 }).await;

    // Cancel while running: the discard waits for the run to return.
    let staging = env.staging.join(&id);
    assert!(staging.join("part.bin").exists());
    let resp = env
        .call("POST", &format!("/api/v1/jobs/{id}/cancel"), "")
        .await;
    let cancelled: JobSnapshot = json(resp).await;
    assert_eq!(cancelled.state, reelarr_types::JobState::Cancelled);
    wait("staging removed", || async { !staging.exists() }).await;
    let stops = env.script.stops.lock().unwrap().clone();
    assert_eq!(stops.iter().filter(|(j, _)| *j == core_id).count(), 2);
    assert!(
        stops.iter().all(|(_, files)| *files),
        "files were removed under a running worker"
    );
    let resp = env
        .call("POST", &format!("/api/v1/jobs/{id}/cancel"), "")
        .await;
    assert_eq!(
        api_error(resp, StatusCode::CONFLICT).await,
        "invalid_transition"
    );
    let resp = env
        .call("POST", &format!("/api/v1/jobs/{id}/resume"), "")
        .await;
    assert_eq!(
        api_error(resp, StatusCode::CONFLICT).await,
        "invalid_transition"
    );
}

#[tokio::test]
async fn cancelling_an_idle_job_discards_its_files_at_once() {
    let env = env(256).await;
    // No NNTP provider: nothing is dispatched.
    let job = env.upload("Idle").await;
    let staging = env.staging.join(&job.id);
    let publish = env
        .staging
        .parent()
        .unwrap()
        .join("complete")
        .join(format!(".reelarr-publish-{}", job.id));
    for dir in [&staging, &publish] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("x"), b"x").unwrap();
    }
    let resp = env
        .call("POST", &format!("/api/v1/jobs/{}/cancel", job.id), "")
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!staging.exists() && !publish.exists());
    assert!(env.script.starts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn priority_and_atomic_reorder() {
    let env = env(256).await;
    let a = env.upload("A").await.id;
    let b = env.upload("B").await.id;
    let c = env.upload("C").await.id;
    let titles = |jobs: Vec<JobSnapshot>| jobs.into_iter().map(|j| j.title).collect::<Vec<_>>();
    assert_eq!(titles(env.list().await), ["A", "B", "C"]);

    let resp = env
        .call(
            "PUT",
            &format!("/api/v1/jobs/{c}/priority"),
            r#"{"priority":"force"}"#,
        )
        .await;
    assert_eq!(json::<JobSnapshot>(resp).await.priority, Priority::Force);
    assert_eq!(titles(env.list().await), ["C", "A", "B"]);
    let resp = env
        .call(
            "PUT",
            &format!("/api/v1/jobs/{c}/priority"),
            r#"{"priority":"urgent"}"#,
        )
        .await;
    assert_eq!(
        api_error(resp, StatusCode::BAD_REQUEST).await,
        "invalid_body"
    );

    let order = |ids: &[&str]| serde_json::json!({ "job_ids": ids }).to_string();
    let resp = env
        .call("PUT", "/api/v1/jobs/order", order(&[&b, &a]))
        .await;
    let listed: JobList = json(resp).await;
    assert_eq!(titles(listed.jobs), ["C", "B", "A"]);
    assert_eq!(titles(env.list().await), ["C", "B", "A"]);

    // A new upload queues behind the reordered jobs.
    env.upload("D").await;
    assert_eq!(titles(env.list().await), ["C", "B", "A", "D"]);

    env.call("POST", &format!("/api/v1/jobs/{a}/cancel"), "")
        .await;
    let before = env.list().await;
    let missing = JobId::new().to_string();
    for (body, status, code) in [
        (order(&[&b, &b]), StatusCode::BAD_REQUEST, "duplicate_job"),
        (
            order(&[&c, &missing]),
            StatusCode::NOT_FOUND,
            "job_not_found",
        ),
        (order(&[&c, &a]), StatusCode::CONFLICT, "job_finished"),
        (
            order(&[&c, "nope"]),
            StatusCode::BAD_REQUEST,
            "invalid_job_id",
        ),
        (order(&[]), StatusCode::BAD_REQUEST, "invalid_field"),
        ("{}".into(), StatusCode::BAD_REQUEST, "invalid_body"),
    ] {
        let resp = env.call("PUT", "/api/v1/jobs/order", body).await;
        assert_eq!(api_error(resp, status).await, code);
        assert_eq!(env.list().await, before, "a rejected reorder wrote");
    }
    let resp = env
        .call(
            "PUT",
            &format!("/api/v1/jobs/{a}/priority"),
            r#"{"priority":"low"}"#,
        )
        .await;
    assert_eq!(api_error(resp, StatusCode::CONFLICT).await, "job_finished");
}

#[tokio::test]
async fn release_enqueue_uses_the_indexer_client_and_its_host_policy() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };
    let env = env(256).await;
    env.configure_nntp().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/getnzb"))
        .and(query_param("apikey", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string(nzb("From.Indexer")))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = IndexerConfig::new(
        "local",
        Url::parse(&server.uri()).unwrap(),
        SecretString::new(API_KEY),
    );
    config.allow_http = true;
    config.minimum_interval = Duration::ZERO;
    config.extra_download_hosts = vec!["cdn.example.net".into()];
    SqliteIndexerStore::new(&env.state.db, env.cipher.clone())
        .upsert(config)
        .await
        .unwrap();
    let request = |indexer: &str, url: &str| {
        serde_json::json!({
            "indexer": indexer, "download_url": url, "title": "Fallback", "category": "movies",
        })
        .to_string()
    };

    // Off-policy hosts are refused before any request (the mock expects one).
    for url in [
        "https://evil.example/getnzb".to_owned(),
        "http://cdn.example.net/getnzb".to_owned(),
        format!("{}/getnzb", server.uri().replace("127.0.0.1", "localhost")),
    ] {
        let resp = env
            .call("POST", "/api/v1/jobs/release", request("local", &url))
            .await;
        assert_eq!(
            api_error(resp, StatusCode::BAD_REQUEST).await,
            "download_url_not_allowed",
            "{url}"
        );
    }
    let resp = env
        .call(
            "POST",
            "/api/v1/jobs/release",
            request("missing", "https://x.example/a"),
        )
        .await;
    assert_eq!(
        api_error(resp, StatusCode::NOT_FOUND).await,
        "indexer_not_found"
    );
    let resp = env
        .call("POST", "/api/v1/jobs/release", r#"{"indexer":"local"}"#)
        .await;
    assert_eq!(
        api_error(resp, StatusCode::BAD_REQUEST).await,
        "invalid_body"
    );

    let url = format!("{}/getnzb?apikey={API_KEY}", server.uri());
    let resp = env
        .call("POST", "/api/v1/jobs/release", request("local", &url))
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = text(resp).await;
    assert_secret_free(&body);
    assert!(!body.contains("getnzb"));
    let job: JobSnapshot = serde_json::from_str(&body).unwrap();
    assert_eq!(job.title, "From.Indexer");
    assert_eq!(job.category.as_deref(), Some("movies"));
    wait("release to start", || async { env.started(&job.id) == 1 }).await;
    assert_secret_free(&text(env.call("GET", "/api/v1/jobs", "").await).await);
}

/// Minimal SSE reader over the response body.
struct Sse {
    body: Body,
    buf: String,
}
impl Sse {
    async fn open(env: &Env) -> Self {
        let resp = env.call("GET", "/api/v1/jobs/events", Body::empty()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        Self {
            body: resp.into_body(),
            buf: String::new(),
        }
    }
    /// Next named frame (keep-alive comments are skipped).
    async fn next(&mut self) -> (String, String) {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let frame: String = self.buf.drain(..end + 2).collect();
                let mut name = String::new();
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(v) = line.strip_prefix("event: ") {
                        name = v.to_owned();
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data += v;
                    } else {
                        assert!(
                            line.is_empty() || line.starts_with(':'),
                            "unexpected SSE line {line:?} (no ids: no durable replay)"
                        );
                    }
                }
                if !name.is_empty() {
                    return (name, data);
                }
                continue;
            }
            let frame = tokio::time::timeout(Duration::from_secs(5), self.body.frame())
                .await
                .expect("SSE frame")
                .unwrap()
                .unwrap();
            if let Ok(data) = frame.into_data() {
                self.buf += std::str::from_utf8(&data).unwrap();
            }
        }
    }
    async fn snapshot(&mut self) -> JobList {
        let (name, data) = self.next().await;
        assert_eq!(name, "snapshot");
        serde_json::from_str(&data).unwrap()
    }
    async fn event(&mut self) -> JobEvent {
        let (name, data) = self.next().await;
        assert_eq!(name, "job", "{data}");
        serde_json::from_str(&data).unwrap()
    }
    async fn quiet(&mut self) {
        let next = tokio::time::timeout(Duration::from_millis(200), self.next()).await;
        assert!(next.is_err(), "unexpected frame {next:?}");
    }
}

#[tokio::test]
async fn sse_sends_a_snapshot_then_seq_events_and_resyncs_after_lag() {
    let env = env(4).await;
    let job = env.upload("Events").await;
    let id: JobId = job.id.parse().unwrap();
    let mut sse = Sse::open(&env).await;
    let snapshot = sse.snapshot().await;
    assert_eq!(snapshot.jobs.len(), 1);
    let watermark = snapshot.jobs[0].last_event_seq;
    assert_eq!(watermark, job.last_event_seq);
    assert_secret_free(&serde_json::to_string(&snapshot).unwrap());

    env.call("POST", &format!("/api/v1/jobs/{}/pause", job.id), "")
        .await;
    let event = sse.event().await;
    assert_eq!(event.job_id, job.id);
    assert_eq!(event.seq, watermark + 1);
    assert_eq!(
        event.kind,
        JobEventKind::StateChanged {
            from: reelarr_types::JobState::Queued,
            to: reelarr_types::JobState::Paused,
        }
    );

    // Overflow the 4-slot hub without reading: the stream resyncs.
    for n in 0..20 {
        env.state
            .jobs
            .message(id, format!("note {n}"))
            .await
            .unwrap();
    }
    let latest = env.state.jobs.get(id).await.unwrap().last_event_seq;
    let resync = sse.snapshot().await;
    assert_eq!(resync.jobs[0].last_event_seq, latest);
    assert_eq!(resync.jobs[0].state, reelarr_types::JobState::Paused);
    // Buffered events at or below the new watermark are dropped.
    sse.quiet().await;
    env.state.jobs.message(id, "after".into()).await.unwrap();
    let event = sse.event().await;
    assert_eq!(event.seq, latest + 1);
    assert_eq!(
        event.kind,
        JobEventKind::Message {
            message: "after".into()
        }
    );

    // A new job's Created event reaches an existing stream.
    let other = env.upload("Second").await;
    let event = sse.event().await;
    assert_eq!(event.job_id, other.id);
    assert_eq!(event.kind, JobEventKind::Created);

    // A reconnect starts from a fresh snapshot, not a replay.
    drop(sse);
    let mut again = Sse::open(&env).await;
    let fresh = again.snapshot().await;
    assert_eq!(fresh.jobs.len(), 2);
    again.quiet().await;
}
