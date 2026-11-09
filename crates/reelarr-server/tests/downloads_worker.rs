//! The supervisor over the real NZB worker (`NzbRunnerFactory`) with an
//! in-memory article source per NNTP settings generation: PAR2 verification
//! is wired, recovery paths land on the job row, and idle drain keeps a
//! running download on its original source.
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use reelarr_core::{
    CompletedDownload, EventHub, FailureCode, ImportHandler, ImportResult, JobId, JobState,
    JobStore,
};
use reelarr_db::{
    Database, SqliteJobStore, SqliteNntpStore, SqliteNzbStore, secrets::XChaChaSecretCipher,
};
use reelarr_nzb::{
    Segment, nntp,
    worker::{ArticleSource, NzbWorker, Par2Verify, REPAIR_UNAVAILABLE, WorkerConfig},
};
use reelarr_server::downloads::{
    ArticleConnector, NzbRunnerFactory, Supervisor, SupervisorConfig, articles_per_job,
    server_worker,
};
use reelarr_types::{MediaFileId, MovieId, nntp::NntpServerUpdate};
use tokio::sync::{Notify, Semaphore};

const PART: usize = 1000;

fn yenc(data: &[u8], name: &str, part: usize, total: usize, start: usize, full: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "=ybegin part={part} total={total} line=128 size={} name={name}\r\n=ypart begin={} end={}\r\n",
        full.len(),
        start + 1,
        start + data.len()
    )
    .into_bytes();
    let mut line = Vec::new();
    let flush = |line: &mut Vec<u8>, out: &mut Vec<u8>| {
        if line.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.append(line);
        out.extend_from_slice(b"\r\n");
    };
    for &byte in data {
        let b = byte.wrapping_add(42);
        if matches!(b, 0 | 10 | 13 | 61) {
            line.extend([b'=', b.wrapping_add(64)]);
        } else {
            line.push(b);
        }
        if line.len() >= 128 {
            flush(&mut line, &mut out);
        }
    }
    if !line.is_empty() {
        flush(&mut line, &mut out);
    }
    out.extend(
        format!(
            "=yend size={} part={part} pcrc32={:08x} crc32={:08x}\r\n",
            data.len(),
            crc32fast::hash(data),
            crc32fast::hash(full)
        )
        .into_bytes(),
    );
    out
}

/// NZB XML plus its articles. Message IDs are unique per release `tag`.
fn release(
    tag: &str,
    posts: &[(&str, Vec<u8>)],
) -> (String, Vec<Vec<String>>, HashMap<String, Vec<u8>>) {
    let mut xml = format!(
        "<?xml version=\"1.0\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\
         <head><meta type=\"title\">{tag}</meta></head>"
    );
    let mut ids = Vec::new();
    let mut articles = HashMap::new();
    for (i, (name, data)) in posts.iter().enumerate() {
        let parts: Vec<_> = data.chunks(PART).collect();
        xml += &format!(
            "<file poster=\"tester@example.test\" date=\"1700000000\" subject=\"&quot;{name}&quot; yEnc (1/{})\"><groups><group>alt.binaries.test</group></groups><segments>",
            parts.len()
        );
        let mut file_ids = Vec::new();
        for (n, chunk) in parts.iter().enumerate() {
            let id = format!("{tag}-f{i}p{}@test.example", n + 1);
            let body = yenc(chunk, name, n + 1, parts.len(), n * PART, data);
            xml += &format!(
                "<segment bytes=\"{}\" number=\"{}\">{id}</segment>",
                body.len(),
                n + 1
            );
            articles.insert(id.clone(), body);
            file_ids.push(id);
        }
        ids.push(file_ids);
        xml += "</segments></file>";
    }
    xml += "</nzb>";
    (xml, ids, articles)
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn par2_fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../reelarr-par2/tests/fixtures/basic")
            .join(name),
    )
    .unwrap()
}

/// What every generation's source can see, plus which host served what.
struct Usenet {
    articles: Mutex<HashMap<String, Vec<u8>>>,
    fetched: Mutex<Vec<(String, String)>>,
    /// Message IDs whose fetch waits for `release`.
    hang: Mutex<HashSet<String>>,
    hung: Notify,
    release: Semaphore,
    connected: Mutex<Vec<String>>,
    retired: Mutex<Vec<String>>,
}
impl Default for Usenet {
    fn default() -> Self {
        Self {
            articles: Mutex::default(),
            fetched: Mutex::default(),
            hang: Mutex::default(),
            hung: Notify::new(),
            release: Semaphore::new(0),
            connected: Mutex::default(),
            retired: Mutex::default(),
        }
    }
}
impl Usenet {
    fn hosts_for(&self, ids: &[String]) -> HashSet<String> {
        self.fetched
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, id)| ids.contains(id))
            .map(|(host, _)| host.clone())
            .collect()
    }
}

#[derive(Clone)]
struct Source {
    host: String,
    usenet: Arc<Usenet>,
}
impl ArticleSource for Source {
    async fn fetch(&self, segment: &Segment) -> Result<Vec<u8>, nntp::Error> {
        let id = segment.message_id.clone();
        self.usenet
            .fetched
            .lock()
            .unwrap()
            .push((self.host.clone(), id.clone()));
        if self.usenet.hang.lock().unwrap().contains(&id) {
            self.usenet.hung.notify_one();
            self.usenet.release.acquire().await.unwrap().forget();
        }
        self.usenet
            .articles
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(nntp::Error::NotFound)
    }
}

struct Connector(Arc<Usenet>);
impl ArticleConnector for Connector {
    type Source = Source;
    async fn connect(&self, config: nntp::ServerConfig) -> Result<Source, nntp::Error> {
        self.0.connected.lock().unwrap().push(config.host.clone());
        Ok(Source {
            host: config.host,
            usenet: self.0.clone(),
        })
    }
    async fn retire(source: &Source) {
        source
            .usenet
            .retired
            .lock()
            .unwrap()
            .push(source.host.clone());
    }
}

#[derive(Default)]
struct Imports(Mutex<Vec<CompletedDownload>>);
#[async_trait]
impl ImportHandler for Imports {
    async fn import(&self, download: CompletedDownload) -> ImportResult {
        self.0.lock().unwrap().push(download);
        ImportResult::Imported {
            media_file_id: MediaFileId(1),
        }
    }
}

struct Env {
    max_jobs: usize,
    _dir: tempfile::TempDir,
    output: PathBuf,
    staging: PathBuf,
    jobs: Arc<SqliteJobStore>,
    nzbs: SqliteNzbStore,
    nntp: SqliteNntpStore,
    usenet: Arc<Usenet>,
    supervisor: Supervisor,
}

async fn env(max_jobs: usize) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(dir.path().join("reelarr.db"))
        .await
        .unwrap();
    let jobs = Arc::new(SqliteJobStore::with_options(
        &db,
        EventHub::default(),
        Duration::ZERO,
    ));
    let nzbs = SqliteNzbStore::new(&db);
    let nntp = SqliteNntpStore::new(&db, Arc::new(XChaChaSecretCipher::ephemeral().unwrap()));
    let staging = dir.path().join("incomplete");
    let output = dir.path().join("complete");
    let usenet = Arc::new(Usenet::default());
    let factory = NzbRunnerFactory::new(
        jobs.clone(),
        nzbs.clone(),
        Connector(usenet.clone()),
        WorkerConfig::new(&staging, &output),
        max_jobs,
    );
    let supervisor = Supervisor::new(
        jobs.clone(),
        nzbs.clone(),
        nntp.clone(),
        Arc::new(factory),
        WorkerConfig::new(&staging, &output),
        SupervisorConfig {
            max_concurrent_jobs: max_jobs,
            poll_interval: Duration::from_millis(50),
            ..SupervisorConfig::default()
        },
    );
    Env {
        max_jobs,
        _dir: dir,
        output,
        staging,
        jobs,
        nzbs,
        nntp,
        usenet,
        supervisor,
    }
}

impl Env {
    /// A second supervisor over the same database, as after a restart.
    fn restarted(&self) -> Supervisor {
        let factory = NzbRunnerFactory::new(
            self.jobs.clone(),
            self.nzbs.clone(),
            Connector(self.usenet.clone()),
            WorkerConfig::new(&self.staging, &self.output),
            self.max_jobs,
        );
        Supervisor::new(
            self.jobs.clone(),
            self.nzbs.clone(),
            self.nntp.clone(),
            Arc::new(factory),
            WorkerConfig::new(&self.staging, &self.output),
            SupervisorConfig {
                max_concurrent_jobs: self.max_jobs,
                poll_interval: Duration::from_millis(50),
                ..SupervisorConfig::default()
            },
        )
    }
    async fn configure(&self, host: &str) {
        self.nntp.save(&NntpServerUpdate::new(host)).await.unwrap();
        self.supervisor.nntp_settings_changed();
    }
    /// Admission the way the job API (PR 5) will do it.
    async fn enqueue(&self, nzb: &str, movie: Option<MovieId>) -> JobId {
        let admit = server_worker(
            self.jobs.clone(),
            self.nzbs.clone(),
            Source {
                host: "admission-only".into(),
                usenet: self.usenet.clone(),
            },
            WorkerConfig::new(&self.staging, &self.output),
        );
        let job = admit
            .enqueue(nzb.as_bytes(), "fallback", None, movie)
            .await
            .unwrap();
        self.supervisor.wake();
        job.id
    }
    fn add(&self, articles: HashMap<String, Vec<u8>>) {
        self.usenet.articles.lock().unwrap().extend(articles);
    }
    async fn wait_state(&self, id: JobId, state: JobState) {
        for _ in 0..1000 {
            if self.jobs.get(id).await.unwrap().state == state {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let now = self.jobs.get(id).await.unwrap();
        panic!(
            "{id} never reached {state:?}; it is {:?} ({:?})",
            now.state, now.failure
        );
    }
}

#[test]
fn the_server_worker_type_uses_par2_verify() {
    // Compile-time: the parity backend is part of the server worker's type.
    fn assert_par2<S, A, R>(_: &NzbWorker<S, A, Par2Verify, R>) {}
    let _ = |w: &reelarr_server::downloads::ServerWorker<Source>| assert_par2(w);
}

#[test]
fn connections_are_split_between_concurrent_jobs() {
    assert_eq!(articles_per_job(8, 2, 8), 4);
    assert_eq!(articles_per_job(20, 2, 8), 8);
    assert_eq!(articles_per_job(1, 3, 8), 1);
    assert_eq!(articles_per_job(10, 0, 8), 8);
}

#[tokio::test]
async fn a_damaged_release_is_checked_with_par2() {
    let e = env(1).await;
    let posts = [
        ("movie.mkv", par2_fixture("movie.mkv")),
        ("notes.nfo", par2_fixture("notes.nfo")),
        ("basic.par2", par2_fixture("basic.par2")),
        ("basic.vol00+04.par2", par2_fixture("basic.vol00+04.par2")),
    ];
    let (nzb, ids, mut articles) = release("par2", &posts);
    // Article 6 is bytes 5000..6000: only PAR2 slice 1 is damaged.
    articles.remove(&ids[0][5]);
    e.add(articles);
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let job = e.enqueue(&nzb, None).await;
    e.wait_state(job, JobState::Failed).await;
    let failure = e.jobs.get(job).await.unwrap().failure.unwrap();
    assert_eq!(failure.code, FailureCode::RepairFailed);
    assert!(failure.message.starts_with(REPAIR_UNAVAILABLE));
    // Only Par2Verify measures the damage; NoParity says repair is unsupported.
    assert!(
        failure
            .message
            .contains("needs 1 recovery block and the release has 4"),
        "{}",
        failure.message
    );
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn idle_drain_finishes_on_the_old_source_and_new_jobs_use_the_new_one() {
    let e = env(2).await;
    let imports = Arc::new(Imports::default());
    e.supervisor.set_import_handler(imports.clone());
    let (nzb_a, ids_a, articles_a) = release("first", &[("First.mkv", payload(3500, 1))]);
    let (nzb_b, ids_b, articles_b) = release("second", &[("Second.mkv", payload(2500, 2))]);
    e.add(articles_a);
    e.add(articles_b);
    // The first job stalls on its last article until released.
    e.usenet.hang.lock().unwrap().insert(ids_a[0][3].clone());

    e.configure("news.old.test").await;
    e.supervisor.start().await.unwrap();
    let a = e.enqueue(&nzb_a, Some(MovieId(7))).await;
    e.usenet.hung.notified().await;
    assert_eq!(e.jobs.get(a).await.unwrap().state, JobState::Downloading);
    // The staging directory is recorded as soon as downloading starts.
    assert_eq!(
        e.jobs.recovery_path(a).await.unwrap(),
        Some(e.staging.join(a.to_string()))
    );

    e.configure("news.new.test").await;
    let b = e.enqueue(&nzb_b, Some(MovieId(8))).await;
    e.wait_state(b, JobState::Completed).await;
    assert_eq!(
        *e.usenet.connected.lock().unwrap(),
        ["news.old.test", "news.new.test"]
    );
    assert_eq!(
        e.usenet.hosts_for(&ids_b[0]),
        HashSet::from(["news.new.test".to_owned()])
    );
    // The first job was not interrupted, and its pool is still open.
    assert_eq!(e.jobs.get(a).await.unwrap().state, JobState::Downloading);
    assert!(e.usenet.retired.lock().unwrap().is_empty());

    e.usenet.release.add_permits(1);
    e.wait_state(a, JobState::Completed).await;
    assert_eq!(
        e.usenet.hosts_for(&ids_a[0]),
        HashSet::from(["news.old.test".to_owned()])
    );
    for _ in 0..200 {
        if !e.usenet.retired.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(*e.usenet.retired.lock().unwrap(), ["news.old.test"]);

    // Published folders were recorded as the recovery path and imported.
    let imported = imports.0.lock().unwrap().clone();
    assert_eq!(imported.len(), 2);
    for (id, movie) in [(a, MovieId(7)), (b, MovieId(8))] {
        let download = imported.iter().find(|d| d.job_id == id).unwrap();
        assert_eq!(download.movie_id, Some(movie));
        assert!(download.output_path.starts_with(&e.output));
        assert_eq!(
            e.jobs.recovery_path(id).await.unwrap().as_deref(),
            Some(download.output_path.as_path())
        );
    }
    e.supervisor.shutdown().await;
    assert_eq!(
        *e.usenet.retired.lock().unwrap(),
        ["news.old.test", "news.new.test"]
    );
}

#[tokio::test]
async fn without_an_import_handler_a_download_stops_at_ready_for_import() {
    let e = env(1).await;
    let (nzb, _, articles) = release("plain", &[("Plain.mkv", payload(1800, 3))]);
    e.add(articles);
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let job = e.enqueue(&nzb, Some(MovieId(1))).await;
    e.wait_state(job, JobState::ReadyForImport).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        e.jobs.get(job).await.unwrap().state,
        JobState::ReadyForImport
    );
    e.supervisor.shutdown().await;
}

#[tokio::test]
async fn shutdown_mid_download_resumes_after_restart_recovery() {
    let e = env(1).await;
    let (nzb, ids, articles) = release("resume", &[("Resume.mkv", payload(2500, 4))]);
    e.add(articles);
    e.usenet.hang.lock().unwrap().insert(ids[0][2].clone());
    e.configure("news.test").await;
    e.supervisor.start().await.unwrap();
    let job = e.enqueue(&nzb, None).await;
    e.usenet.hung.notified().await;
    e.supervisor.shutdown().await;
    assert_eq!(e.jobs.get(job).await.unwrap().state, JobState::Downloading);
    assert!(e.supervisor.active().is_empty());
    assert_eq!(*e.usenet.retired.lock().unwrap(), ["news.test"]);

    // Next start: recovery requeues the job before it is dispatched again,
    // and the run completes from the same NZB.
    e.usenet.hang.lock().unwrap().clear();
    let restarted = e.restarted();
    restarted.start().await.unwrap();
    e.wait_state(job, JobState::ReadyForImport).await;
    let job = e.jobs.get(job).await.unwrap();
    assert_eq!(job.attempts, 2, "requeued to Queued, then downloaded again");
    restarted.shutdown().await;
}
