//! Default import folder (#100), end to end through `AppState` with the real
//! `MediaImportHandler`: a finished download that cannot choose a library
//! folder waits in `ReadyForImport` (retryable `NoDefaultRoot`), and picking
//! the default or adding the first folder imports it at once instead of
//! after the retry wait.
use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use reelarr_core::{JobEventKind, JobId, JobState, JobStore};
use reelarr_db::{Database, SqliteRecoveryPaths, secrets::XChaChaSecretCipher};
use reelarr_media::db::SqliteMovieStore;
use reelarr_media::store::MovieStore;
use reelarr_nzb::worker::RecoveryPaths;
use reelarr_server::{
    app,
    downloads::{NewDownload, SupervisorConfig},
    state::{AppState, DownloadOptions},
};
use reelarr_types::{Artwork, ExternalIds, LibraryRootInfo, MovieId, MovieMetadata, SessionInfo};
use tower::ServiceExt;

const NZB: &str = "<?xml version=\"1.0\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\
    <head><meta type=\"title\">Waiting.Movie.2020.1080p</meta></head>\
    <file poster=\"tester@example.test\" date=\"1700000000\" subject=\"&quot;a.mkv&quot; yEnc (1/1)\">\
    <groups><group>alt.binaries.test</group></groups>\
    <segments><segment bytes=\"10\" number=\"1\">a@test.example</segment></segments></file></nzb>";

/// Longer than any test: a job only imports again if the wait is dropped.
const LONG: Duration = Duration::from_secs(3600);

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    app.clone()
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

async fn bytes(resp: Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

fn cookies(resp: &Response) -> String {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|h| h.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("; ")
}

struct Env {
    tmp: tempfile::TempDir,
    state: AppState,
    app: Router,
    cookie: String,
    csrf: String,
    movie: MovieId,
}

async fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::connect(tmp.path().join("reelarr.db"))
        .await
        .unwrap();
    let mut options = DownloadOptions::new(tmp.path().join("downloads"));
    // Only wake() dispatches, and a held import is not retried in a test.
    options.scheduler = SupervisorConfig {
        poll_interval: LONG,
        import_retry: vec![LONG],
        ..SupervisorConfig::default()
    };
    let state = AppState::with_options(
        db,
        Arc::new(XChaChaSecretCipher::ephemeral().unwrap()),
        options,
    );
    let movie = SqliteMovieStore::new(&state.db)
        .insert(
            MovieMetadata {
                external_ids: ExternalIds {
                    tmdb_id: 4242,
                    imdb_id: None,
                },
                title: "Waiting Movie".into(),
                original_title: None,
                year: Some(2020),
                overview: None,
                runtime_minutes: None,
                artwork: Artwork::default(),
            },
            0,
        )
        .await
        .unwrap()
        .id;
    let app = app(state.clone());
    let resp = call(&app, "GET", "/api/v1/auth/session", &[], "").await;
    let guest = cookies(&resp);
    let info: SessionInfo = serde_json::from_slice(&bytes(resp).await).unwrap();
    let resp = call(
        &app,
        "POST",
        "/api/v1/auth/setup",
        &[("cookie", &guest), ("x-csrf-token", &info.csrf_token)],
        r#"{"username":"admin","password":"a-long-password"}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = cookies(&resp);
    let resp = call(
        &app,
        "GET",
        "/api/v1/auth/session",
        &[("cookie", &cookie)],
        "",
    )
    .await;
    let csrf = serde_json::from_slice::<SessionInfo>(&bytes(resp).await)
        .unwrap()
        .csrf_token;
    Env {
        tmp,
        state,
        app,
        cookie,
        csrf,
        movie,
    }
}

impl Env {
    async fn write(&self, method: &str, uri: &str, body: &str) -> Response {
        call(
            &self.app,
            method,
            uri,
            &[("cookie", &self.cookie), ("x-csrf-token", &self.csrf)],
            body,
        )
        .await
    }

    /// Registers an existing folder as a library root.
    async fn add_root(&self, name: &str) -> LibraryRootInfo {
        let dir = self.tmp.path().join("media").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let body = format!(r#"{{"path":"{}","kind":"library"}}"#, dir.display());
        let resp = self.write("POST", "/api/v1/library/roots", &body).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        serde_json::from_slice(&bytes(resp).await).unwrap()
    }

    /// A finished download for the movie, waiting in `ReadyForImport` as the
    /// NZB worker leaves it, with one video above the sample threshold.
    async fn finished_download(&self) -> JobId {
        let job = self
            .state
            .downloads
            .enqueue_nzb(
                NZB.as_bytes().to_vec(),
                NewDownload {
                    fallback_title: "fallback".into(),
                    movie_id: Some(self.movie),
                    ..NewDownload::default()
                },
            )
            .await
            .unwrap();
        for to in [
            JobState::Downloading,
            JobState::Verifying,
            JobState::ReadyForImport,
        ] {
            self.state.jobs.transition(job.id, to, None).await.unwrap();
        }
        let published: PathBuf = self
            .tmp
            .path()
            .join("downloads/complete/Waiting.Movie.2020.1080p");
        std::fs::create_dir_all(&published).unwrap();
        // Sparse: above the 50 MiB sample limit without writing it.
        std::fs::File::create(published.join("Waiting.Movie.2020.1080p.mkv"))
            .unwrap()
            .set_len(60 * 1024 * 1024)
            .unwrap();
        SqliteRecoveryPaths::new((*self.state.jobs).clone())
            .set_recovery_path(job.id, Some(&published))
            .await
            .unwrap();
        job.id
    }

    async fn state_of(&self, id: JobId) -> JobState {
        self.state.jobs.get(id).await.unwrap().state
    }

    async fn wait_state(&self, id: JobId, want: JobState) {
        for _ in 0..1000 {
            if self.state_of(id).await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "{id} never reached {want:?}; it is {:?}",
            self.state_of(id).await
        );
    }

    async fn imported_root(&self) -> Option<i64> {
        reelarr_media::db::SqliteMediaFileStore::new(&self.state.db)
            .list_by_movie(self.movie)
            .await
            .unwrap()
            .first()
            .map(|f| f.root_id.0)
    }
}

/// Waits until an import attempt was turned into a retry, then for the
/// import registry to empty, and checks the job stays put.
async fn waits_for_a_folder(
    env: &Env,
    id: JobId,
    events: &mut tokio::sync::broadcast::Receiver<reelarr_core::JobEvent>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("no import retry message")
            .unwrap();
        if event.job_id == id
            && let JobEventKind::Message { message } = event.kind
            && message.contains("Import will be retried")
        {
            break;
        }
    }
    for _ in 0..500 {
        if env.state.downloads.importing().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Several polls later it is still waiting, not Failed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let job = env.state.jobs.get(id).await.unwrap();
    assert_eq!(job.state, JobState::ReadyForImport);
    assert!(job.failure.is_none(), "{:?}", job.failure);
    assert_eq!(env.imported_root().await, None);
}

#[tokio::test]
async fn several_folders_wait_for_a_default_then_import_at_once() {
    let env = env().await;
    env.add_root("movies").await;
    let tv = env.add_root("tvshows").await;
    env.add_root("music").await;
    let id = env.finished_download().await;
    let mut events = env.state.jobs.events().subscribe();
    env.state.downloads.start().await.unwrap();
    waits_for_a_folder(&env, id, &mut events).await;

    // Picking the default drops the one-hour wait and wakes the importer.
    let uri = format!("/api/v1/library/roots/{}/default", tv.root.id.0);
    let resp = env.write("PUT", &uri, "").await;
    assert_eq!(resp.status(), StatusCode::OK);
    env.wait_state(id, JobState::Completed).await;
    assert_eq!(env.imported_root().await, Some(tv.root.id.0));
    env.state.downloads.shutdown().await;
}

#[tokio::test]
async fn no_folder_waits_then_the_first_folder_imports_at_once() {
    let env = env().await;
    let id = env.finished_download().await;
    let mut events = env.state.jobs.events().subscribe();
    env.state.downloads.start().await.unwrap();
    waits_for_a_folder(&env, id, &mut events).await;

    // One library folder is used without a stored default.
    let movies = env.add_root("movies").await;
    assert!(!movies.root.is_default);
    env.wait_state(id, JobState::Completed).await;
    assert_eq!(env.imported_root().await, Some(movies.root.id.0));
    env.state.downloads.shutdown().await;
}
