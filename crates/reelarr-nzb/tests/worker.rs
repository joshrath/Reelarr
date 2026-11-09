//! End-to-end worker tests over an in-memory article source. Articles are
//! yEnc-encoded here from the 1.3 arithmetic; RAR inputs are the committed
//! reelarr-rar fixtures (see that crate's PROVENANCE.md).
use reelarr_core::jobs::{
    CleanupRegistry, CompletedDownload, DownloadSource, EventHub, FailureCode, ImportHandler,
    ImportResult, InMemoryJobStore, JobId, JobState, JobStore, PathMapping, run_import,
};
use reelarr_nzb::{
    InMemoryNzbStore, Segment, nntp,
    worker::{
        ArticleSource, CancelToken, NzbWorker, Outcome, Par2Verify, ParityBackend, ParityVerdict,
        REPAIR_UNAVAILABLE, RecoveryPathError, RecoveryPaths, RepairOutcome, RepairRequest,
        WorkerConfig,
    },
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

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

/// One posted file: (subject name, contents).
struct Post {
    name: String,
    data: Vec<u8>,
}
fn post(name: &str, data: Vec<u8>) -> Post {
    Post {
        name: name.into(),
        data,
    }
}
fn ids(file: usize, post: &Post) -> Vec<String> {
    (0..post.data.len().div_ceil(PART))
        .map(|p| {
            format!(
                "f{file}p{}@{}.test",
                p + 1,
                post.name.replace(['+', ' '], "_")
            )
        })
        .collect()
}
/// Build NZB XML and the article map.
fn release(title: &str, posts: &[Post]) -> (String, HashMap<String, Vec<u8>>) {
    let mut xml = format!(
        "<?xml version=\"1.0\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\
         <head><meta type=\"title\">{title}</meta></head>"
    );
    let mut articles = HashMap::new();
    for (i, p) in posts.iter().enumerate() {
        let parts: Vec<_> = p.data.chunks(PART).collect();
        xml += &format!(
            "<file poster=\"tester@example.test\" date=\"1700000000\" subject=\"&quot;{}&quot; yEnc (1/{})\"><groups><group>alt.binaries.test</group></groups><segments>",
            p.name,
            parts.len()
        );
        for (n, (chunk, id)) in parts.iter().zip(ids(i, p)).enumerate() {
            let body = yenc(chunk, &p.name, n + 1, parts.len(), n * PART, &p.data);
            xml += &format!(
                "<segment bytes=\"{}\" number=\"{}\">{id}</segment>",
                body.len(),
                n + 1
            );
            articles.insert(id, body);
        }
        xml += "</segments></file>";
    }
    xml += "</nzb>";
    (xml, articles)
}

#[derive(Clone, Default)]
struct Source {
    articles: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    fetched: Arc<Mutex<Vec<String>>>,
    hang: Arc<Mutex<HashSet<String>>>,
    hung: Arc<Notify>,
    auth_fail: Arc<Mutex<bool>>,
}
impl Source {
    fn new(articles: HashMap<String, Vec<u8>>) -> Self {
        Self {
            articles: Arc::new(Mutex::new(articles)),
            ..Self::default()
        }
    }
    fn count(&self, id: &str) -> usize {
        self.fetched
            .lock()
            .unwrap()
            .iter()
            .filter(|f| *f == id)
            .count()
    }
}
impl ArticleSource for Source {
    async fn fetch(&self, segment: &Segment) -> Result<Vec<u8>, nntp::Error> {
        let id = segment.message_id.clone();
        self.fetched.lock().unwrap().push(id.clone());
        if *self.auth_fail.lock().unwrap() {
            return Err(nntp::Error::Authentication { code: 481 });
        }
        if self.hang.lock().unwrap().contains(&id) {
            self.hung.notify_one();
            std::future::pending::<()>().await;
        }
        self.articles
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(nntp::Error::NotFound)
    }
}

type Recorded = Vec<(JobId, Option<PathBuf>)>;
#[derive(Clone, Default)]
struct Paths(Arc<Mutex<Recorded>>);
impl RecoveryPaths for Paths {
    async fn set_recovery_path(
        &self,
        job: JobId,
        path: Option<&Path>,
    ) -> Result<(), RecoveryPathError> {
        self.0.lock().unwrap().push((job, path.map(Path::to_owned)));
        Ok(())
    }
}

struct Env {
    _dir: tempfile::TempDir,
    staging: PathBuf,
    output: PathBuf,
    jobs: Arc<InMemoryJobStore>,
    source: Source,
    paths: Paths,
    worker: NzbWorker<InMemoryNzbStore, Source, reelarr_nzb::worker::NoParity, Paths>,
}
fn env(articles: HashMap<String, Vec<u8>>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("staging");
    let output = dir.path().join("complete");
    let jobs = Arc::new(InMemoryJobStore::new(EventHub::default(), Duration::ZERO));
    let source = Source::new(articles);
    let paths = Paths::default();
    let mut config = WorkerConfig::new(&staging, &output);
    config.max_concurrent_articles = 3;
    let worker = NzbWorker::new(
        jobs.clone(),
        InMemoryNzbStore::default(),
        source.clone(),
        config,
    )
    .with_recovery_paths(paths.clone());
    Env {
        _dir: dir,
        staging,
        output,
        jobs,
        source,
        paths,
        worker,
    }
}
fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}
fn rar_fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../reelarr-rar/tests/fixtures")
            .join(name),
    )
    .unwrap()
}
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}
async fn ready(outcome: Outcome) -> CompletedDownload {
    match outcome {
        Outcome::ReadyForImport(download) => download,
        other => panic!("expected ReadyForImport, got {other:?}"),
    }
}

#[tokio::test]
async fn plain_files_publish_atomically_and_skip_recovery_volumes() {
    let movie = payload(2500, 7);
    let posts = [
        post("Movie.mkv", movie.clone()),
        post("Movie.par2", payload(300, 1)),
        post("Movie.vol00+01.par2", payload(1200, 2)),
    ];
    let (nzb, articles) = release("Movie (2024) 1080p", &posts);
    let e = env(articles);
    let job = e
        .worker
        .enqueue(
            nzb.as_bytes(),
            "fallback",
            Some("movies".into()),
            Some(reelarr_types::MovieId(42)),
        )
        .await
        .unwrap();
    assert_eq!(job.title, "Movie (2024) 1080p");
    let download = ready(e.worker.run(job.id, &CancelToken::new()).await.unwrap()).await;

    assert_eq!(download.output_path, e.output.join("Movie (2024) 1080p"));
    assert_eq!(download.category.as_deref(), Some("movies"));
    assert_eq!(download.movie_id, Some(reelarr_types::MovieId(42)));
    assert_eq!(download.source, DownloadSource::BuiltinNzb);
    assert_eq!(listing(&download.output_path), ["Movie.mkv"]);
    assert_eq!(
        fs::read(download.output_path.join("Movie.mkv")).unwrap(),
        movie
    );
    // Only the final folder is visible; no publish temp dir remains.
    assert_eq!(listing(&e.output), ["Movie (2024) 1080p"]);
    for id in ids(2, &posts[2]) {
        assert_eq!(
            e.source.count(&id),
            0,
            "recovery volume must not be fetched"
        );
    }
    let snapshot = e.jobs.get(job.id).await.unwrap();
    assert_eq!(snapshot.state, JobState::ReadyForImport);
    assert_eq!(snapshot.progress.bytes_done, snapshot.progress.bytes_total);
    let recorded = e.paths.0.lock().unwrap().clone();
    assert_eq!(
        recorded.first().unwrap().1.as_deref(),
        Some(e.staging.join(job.id.to_string()).as_path())
    );
    assert_eq!(
        recorded.last().unwrap().1.as_deref(),
        Some(download.output_path.as_path())
    );
}

#[tokio::test]
async fn stored_rar_volumes_extract_into_the_published_folder() {
    let posts: Vec<_> = (1..=3)
        .map(|n| {
            let name = format!("real5.part{n}.rar");
            post(&name, rar_fixture(&name))
        })
        .chain([post("readme.nfo", b"notes".to_vec())])
        .collect();
    let (nzb, articles) = release("Rar Release", &posts);
    let e = env(articles);
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let download = ready(e.worker.run(job.id, &CancelToken::new()).await.unwrap()).await;
    assert_eq!(
        listing(&download.output_path),
        ["payload.txt", "readme.nfo"]
    );
    let expected: Vec<u8> = (0..=255u8).cycle().take(10_240).collect();
    assert_eq!(
        fs::read(download.output_path.join("payload.txt")).unwrap(),
        expected
    );
}

#[tokio::test]
async fn old_style_rar_names_extract() {
    let posts: Vec<_> = ["old4.rar", "old4.r00", "old4.r01"]
        .iter()
        .map(|n| post(n, rar_fixture(n)))
        .collect();
    let (nzb, articles) = release("Old Names", &posts);
    let e = env(articles);
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let download = ready(e.worker.run(job.id, &CancelToken::new()).await.unwrap()).await;
    assert_eq!(listing(&download.output_path), ["payload.txt"]);
}

#[tokio::test]
async fn missing_articles_fail_as_repair_unavailable_and_retry_reuses_completed_files() {
    let posts = [
        post("a.mkv", payload(2500, 3)),
        post("b.nfo", payload(500, 4)),
    ];
    let (nzb, mut articles) = release("Damaged", &posts);
    let gone = ids(0, &posts[0])[1].clone();
    let saved = articles.remove(&gone).unwrap();
    let e = env(articles);
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();

    let Outcome::Failed(failure) = e.worker.run(job.id, &CancelToken::new()).await.unwrap() else {
        panic!("expected failure");
    };
    assert_eq!(failure.code, FailureCode::RepairFailed);
    assert!(!failure.retryable);
    assert!(
        failure.message.starts_with(REPAIR_UNAVAILABLE),
        "{}",
        failure.message
    );
    assert!(
        failure.message.contains("1 of 2 files"),
        "{}",
        failure.message
    );
    let snapshot = e.jobs.get(job.id).await.unwrap();
    assert_eq!(snapshot.state, JobState::Failed);
    assert_eq!(snapshot.failure.as_ref(), Some(&failure));
    assert_eq!(snapshot.progress.articles_missing, 1);
    // The complete file stays on disk; nothing was published.
    let files = e.staging.join(job.id.to_string()).join("files");
    assert_eq!(listing(&files), ["1"]);
    assert!(listing(&e.output).is_empty());
    // The damaged file is kept with its good bytes and a zero-filled hole.
    let partial = e.staging.join(job.id.to_string()).join("partial/0/a.mkv");
    let kept = fs::read(&partial).unwrap();
    let original = &posts[0].data;
    assert_eq!(kept.len(), original.len());
    assert_eq!(&kept[..PART], &original[..PART]);
    assert!(kept[PART..2 * PART].iter().all(|&b| b == 0));
    assert_eq!(&kept[2 * PART..], &original[2 * PART..]);

    // The article shows up later; re-queue fetches only the damaged file.
    e.source.articles.lock().unwrap().insert(gone, saved);
    e.jobs
        .transition(job.id, JobState::Queued, None)
        .await
        .unwrap();
    let download = ready(e.worker.run(job.id, &CancelToken::new()).await.unwrap()).await;
    assert_eq!(listing(&download.output_path), ["a.mkv", "b.nfo"]);
    assert_eq!(
        e.source.count(&ids(1, &posts[1])[0]),
        1,
        "completed file was reused"
    );
    assert_eq!(
        e.source.count(&ids(0, &posts[0])[0]),
        2,
        "damaged file was re-fetched"
    );
}

#[tokio::test]
async fn corrupted_article_counts_as_damage() {
    let posts = [post("a.bin", payload(1500, 9))];
    let (nzb, mut articles) = release("Corrupt", &posts);
    let id = ids(0, &posts[0])[0].clone();
    let body = articles.get_mut(&id).unwrap();
    let at = body.len() / 2;
    body[at] ^= 0x01;
    let e = env(articles);
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let Outcome::Failed(failure) = e.worker.run(job.id, &CancelToken::new()).await.unwrap() else {
        panic!("expected failure");
    };
    assert_eq!(failure.code, FailureCode::RepairFailed);
}

#[tokio::test]
async fn pause_stops_without_publishing_and_resume_finishes() {
    let posts = [post("a.mkv", payload(3500, 5))];
    let (nzb, articles) = release("Pausable", &posts);
    let slow = ids(0, &posts[0])[3].clone();
    let e = Arc::new(env(articles));
    e.source.hang.lock().unwrap().insert(slow.clone());
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let cancel = CancelToken::new();
    let run = {
        let (e, cancel) = (e.clone(), cancel.clone());
        tokio::spawn(async move { e.worker.run(job.id, &cancel).await })
    };
    e.source.hung.notified().await;
    e.jobs
        .transition(job.id, JobState::Paused, None)
        .await
        .unwrap();
    cancel.cancel();
    assert_eq!(run.await.unwrap().unwrap(), Outcome::Stopped);
    assert_eq!(e.jobs.get(job.id).await.unwrap().state, JobState::Paused);
    assert!(listing(&e.output).is_empty());
    let staging = e.staging.join(job.id.to_string());
    assert!(
        listing(&staging)
            .iter()
            .all(|n| !n.starts_with(".reelarr-assembly-")),
        "partial assembly removed: {:?}",
        listing(&staging)
    );

    e.source.hang.lock().unwrap().clear();
    e.jobs.resume(job.id).await.unwrap();
    let download = ready(e.worker.run(job.id, &CancelToken::new()).await.unwrap()).await;
    assert_eq!(listing(&download.output_path), ["a.mkv"]);
}

#[tokio::test]
async fn cancelled_job_is_left_alone_and_discard_removes_staging() {
    let posts = [post("a.mkv", payload(2500, 5))];
    let (nzb, articles) = release("Cancel me", &posts);
    let slow = ids(0, &posts[0])[2].clone();
    let e = Arc::new(env(articles));
    e.source.hang.lock().unwrap().insert(slow);
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let cancel = CancelToken::new();
    let run = {
        let (e, cancel) = (e.clone(), cancel.clone());
        tokio::spawn(async move { e.worker.run(job.id, &cancel).await })
    };
    e.source.hung.notified().await;
    e.jobs
        .transition(job.id, JobState::Cancelled, None)
        .await
        .unwrap();
    cancel.cancel();
    assert_eq!(run.await.unwrap().unwrap(), Outcome::Stopped);
    assert_eq!(e.jobs.get(job.id).await.unwrap().state, JobState::Cancelled);
    e.worker.discard(job.id).await.unwrap();
    assert!(!e.staging.join(job.id.to_string()).exists());
    e.worker.discard(job.id).await.unwrap();
}

#[tokio::test]
async fn server_login_rejection_is_server_auth() {
    let posts = [post("a.mkv", payload(800, 1))];
    let (nzb, articles) = release("Auth", &posts);
    let e = env(articles);
    *e.source.auth_fail.lock().unwrap() = true;
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let Outcome::Failed(failure) = e.worker.run(job.id, &CancelToken::new()).await.unwrap() else {
        panic!("expected failure");
    };
    assert_eq!(failure.code, FailureCode::ServerAuth);
    assert!(failure.retryable);
}

#[tokio::test]
async fn obfuscated_rar_volume_is_not_published() {
    let posts = [post("a1b2c3d4", rar_fixture("real5.part1.rar"))];
    let (nzb, articles) = release("Obfuscated", &posts);
    let e = env(articles);
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let Outcome::Failed(failure) = e.worker.run(job.id, &CancelToken::new()).await.unwrap() else {
        panic!("expected failure");
    };
    assert_eq!(failure.code, FailureCode::ExtractFailed);
    assert!(listing(&e.output).is_empty());
}

#[tokio::test]
async fn incomplete_volume_set_fails_extraction() {
    let posts: Vec<_> = (1..=2)
        .map(|n| {
            let name = format!("real5.part{n}.rar");
            post(&name, rar_fixture(&name))
        })
        .collect();
    let (nzb, articles) = release("Short set", &posts);
    let e = env(articles);
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let Outcome::Failed(failure) = e.worker.run(job.id, &CancelToken::new()).await.unwrap() else {
        panic!("expected failure");
    };
    assert_eq!(failure.code, FailureCode::ExtractFailed);
    assert!(listing(&e.output).is_empty());
    let extras: Vec<_> = listing(&e.staging.join(job.id.to_string()))
        .into_iter()
        .filter(|n| n != "files")
        .collect();
    assert!(extras.is_empty(), "{extras:?}");
}

struct Parity(Mutex<Vec<RepairRequest>>);
impl ParityBackend for Parity {
    async fn verify(&self, _: &RepairRequest, _: &CancelToken) -> ParityVerdict {
        ParityVerdict::Damaged {
            needed: 1,
            available: 0,
        }
    }
    fn can_repair(&self) -> bool {
        true
    }
    async fn repair(&self, request: RepairRequest) -> RepairOutcome {
        self.0.lock().unwrap().push(request);
        RepairOutcome::Failed {
            message: "not enough recovery blocks".into(),
        }
    }
}

#[tokio::test]
async fn available_backend_goes_through_repairing() {
    let posts = [
        post("a.mkv", payload(2500, 3)),
        post("a.vol0+1.par2", payload(100, 1)),
    ];
    let (nzb, mut articles) = release("Repairable", &posts);
    articles.remove(&ids(0, &posts[0])[0]);
    let e = env(articles);
    let parity = Arc::new(Parity(Mutex::new(Vec::new())));
    struct Shared(Arc<Parity>);
    impl ParityBackend for Shared {
        async fn verify(&self, r: &RepairRequest, c: &CancelToken) -> ParityVerdict {
            self.0.verify(r, c).await
        }
        fn can_repair(&self) -> bool {
            self.0.can_repair()
        }
        async fn repair(&self, r: RepairRequest) -> RepairOutcome {
            self.0.repair(r).await
        }
    }
    let worker = e.worker.with_parity(Shared(parity.clone()));
    let job = worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let mut events = e.jobs.events().subscribe();
    let Outcome::Failed(failure) = worker.run(job.id, &CancelToken::new()).await.unwrap() else {
        panic!("expected failure");
    };
    assert_eq!(failure.code, FailureCode::RepairFailed);
    assert_eq!(failure.message, "not enough recovery blocks");
    let requests = parity.0.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].damaged_files, [0]);
    assert_eq!(requests[0].recovery_volumes, [1]);
    assert_eq!(requests[0].bad_articles, 1);
    assert_eq!(requests[0].posted_recovery_blocks, 1);
    assert_eq!(requests[0].partial_files, [0]);
    let mut saw_repairing = false;
    while let Ok(event) = events.try_recv() {
        saw_repairing |= format!("{:?}", event.kind).contains("Repairing");
    }
    assert!(saw_repairing);
}

struct Importer;
#[async_trait::async_trait]
impl ImportHandler for Importer {
    async fn import(&self, download: CompletedDownload) -> ImportResult {
        assert!(download.output_path.join("a.mkv").is_file());
        ImportResult::Imported {
            media_file_id: reelarr_types::MediaFileId(1),
        }
    }
}

#[tokio::test]
async fn import_then_cleanup_removes_staging_and_output() {
    let posts = [post("a.mkv", payload(1200, 8))];
    let (nzb, articles) = release("Importable", &posts);
    let e = env(articles);
    let job = e
        .worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let mut download = ready(e.worker.run(job.id, &CancelToken::new()).await.unwrap()).await;
    download.movie_id = Some(reelarr_types::MovieId(1));
    let mut cleanups = CleanupRegistry::default();
    cleanups.register(DownloadSource::BuiltinNzb, Arc::new(e.worker.cleanup()));
    let output = download.output_path.clone();
    let done = run_import(&*e.jobs, &Importer, &cleanups, download, PathMapping::Local)
        .await
        .unwrap();
    assert_eq!(done.state, JobState::Completed);
    assert!(!output.exists());
    assert!(!e.staging.join(job.id.to_string()).exists());
}

#[tokio::test]
async fn invalid_nzb_creates_no_job() {
    let e = env(HashMap::new());
    assert!(
        e.worker
            .enqueue(b"<nzb>".as_slice(), "x", None, None)
            .await
            .is_err()
    );
    assert!(e.jobs.list().await.unwrap().is_empty());
}

fn par2_fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../reelarr-par2/tests/fixtures/basic")
            .join(name),
    )
    .unwrap()
}

async fn par2_failure(missing: std::ops::Range<usize>) -> (Env, JobId, String) {
    let posts = [
        post("movie.mkv", par2_fixture("movie.mkv")),
        post("notes.nfo", par2_fixture("notes.nfo")),
        post("basic.par2", par2_fixture("basic.par2")),
        post("basic.vol00+04.par2", par2_fixture("basic.vol00+04.par2")),
    ];
    let (nzb, mut articles) = release("Par2 Release", &posts);
    for id in &ids(0, &posts[0])[missing] {
        articles.remove(id);
    }
    let e = env(articles);
    let worker = NzbWorker::new(
        e.jobs.clone(),
        InMemoryNzbStore::default(),
        e.source.clone(),
        WorkerConfig::new(&e.staging, &e.output),
    )
    .with_parity(Par2Verify::default());
    let job = worker
        .enqueue(nzb.as_bytes(), "x", None, None)
        .await
        .unwrap();
    let Outcome::Failed(failure) = worker.run(job.id, &CancelToken::new()).await.unwrap() else {
        panic!("expected failure");
    };
    assert_eq!(failure.code, FailureCode::RepairFailed);
    assert!(!failure.retryable);
    assert!(failure.message.starts_with(REPAIR_UNAVAILABLE));
    let job_dir = e.staging.join(job.id.to_string());
    assert!(!job_dir.join(".reelarr-par2-view").exists());
    (e, job.id, failure.message)
}

#[tokio::test]
async fn par2_verify_reports_a_repairable_release() {
    // Article 6 is bytes 5000..6000: only PAR2 slice 1 is damaged.
    let (e, _, message) = par2_failure(5..6).await;
    assert!(
        message.contains("needs 1 recovery block and the release has 4"),
        "{message}"
    );
    // Recovery volumes are still not downloaded.
    assert!(listing(&e.output).is_empty());
}

#[tokio::test]
async fn par2_verify_reports_an_unrepairable_release() {
    // Bytes 0..21000 cover slices 0 to 5.
    let (_, _, message) = par2_failure(0..21).await;
    assert!(
        message.contains("needs 6 recovery blocks but the release only has 4"),
        "{message}"
    );
}
