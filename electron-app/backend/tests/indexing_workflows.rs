use research_backend::{
    config::StoragePaths,
    graph::MetadataExtractor,
    indexing::{CoordinatedIndex, NativeIndexer, install_commands},
    protocol::ErrorCode,
    providers::EmbeddingClient,
    registry::{BackendFuture, Registry},
    retrieval::EmbeddingIdentity,
    runtime::{Cancellation, serve},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
mod papers_fixtures;
use papers_fixtures::{Page, pdf};

#[derive(Default)]
struct Fake {
    calls: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    fail: AtomicBool,
    panic: AtomicBool,
    hold: AtomicBool,
    hold_after: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl MetadataExtractor for Fake {
    fn extract(&self, _: String, c: Cancellation) -> BackendFuture<Value> {
        let fail = self.fail.load(Ordering::SeqCst);
        Box::pin(async move {
            c.check()?;
            if fail {
                return Err(ErrorCode::Internal);
            }
            Ok(
                json!({"title":"A title","authors":["Shared Author"],"topics":["one","two","three"]}),
            )
        })
    }
}
struct Embeddings(Arc<Fake>);
struct Active(Arc<Fake>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
impl EmbeddingClient for Embeddings {
    fn embed(&self, texts: Vec<String>, c: Cancellation) -> BackendFuture<Vec<Vec<f64>>> {
        let fake = self.0.clone();
        Box::pin(async move {
            let _active = Active(fake.clone());
            let active = fake.active.fetch_add(1, Ordering::SeqCst) + 1;
            fake.peak.fetch_max(active, Ordering::SeqCst);
            let call = fake.calls.fetch_add(1, Ordering::SeqCst) + 1;
            fake.entered.notify_one();
            let hold_after = fake.hold_after.load(Ordering::SeqCst);
            if fake.hold.load(Ordering::SeqCst) || (hold_after > 0 && call >= hold_after) {
                fake.release.notified().await;
            }
            assert!(
                !fake.panic.load(Ordering::SeqCst),
                "synthetic provider panic"
            );
            c.check()?;
            Ok(texts.iter().map(|_| vec![0.25, 0.75]).collect())
        })
    }
}
fn paths(temp: &std::path::Path) -> StoragePaths {
    let data = temp.join("data");
    let papers = temp.join("papers");
    let resources = temp.join("resources");
    for path in [&data, &papers, &resources] {
        std::fs::create_dir(path).unwrap();
    }
    StoragePaths {
        history_database: data.join("history.sqlite3"),
        vector_database: data.join("vectors.sqlite3"),
        graph_database: data.join("graph.sqlite3"),
        index_state_database: data.join("index-state.sqlite3"),
        bm25_index: data.join("bm25.json"),
        evaluation_results: data.join("evaluations"),
        data_root: data,
        papers,
        resources,
    }
}
fn identity() -> EmbeddingIdentity {
    EmbeddingIdentity {
        model: "fake/model/v1".into(),
        dimension: 2,
    }
}
fn text(paths: &StoragePaths, id: &str, content: &str) {
    std::fs::write(
        paths.papers.join(format!("{id}.pdf")),
        pdf(vec![Page::Text(vec![content.as_bytes().to_vec()])]),
    )
    .unwrap();
}
async fn service(paths: &StoragePaths, fake: Arc<Fake>, workers: usize) -> NativeIndexer {
    let index = CoordinatedIndex::open(paths, identity()).await.unwrap();
    index.recover().await.unwrap();
    NativeIndexer::new(index, Arc::new(Embeddings(fake.clone())), fake, workers).unwrap()
}
type Lines = tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>;
struct Client {
    lines: Lines,
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    id: usize,
}
impl Client {
    fn new(index: NativeIndexer) -> Self {
        let mut registry = Registry::foundation();
        install_commands(&mut registry, Ok(index));
        Self::registry(registry)
    }
    fn registry(registry: Registry) -> Self {
        let (client, server) = tokio::io::duplex(1024 * 1024);
        let (r, w) = tokio::io::split(server);
        let server = tokio::spawn(serve(r, w, registry));
        let (r, write) = tokio::io::split(client);
        Self {
            lines: BufReader::new(r).lines(),
            write,
            server,
            id: 0,
        }
    }
    async fn send(&mut self, command: &str, params: Value) -> String {
        self.id += 1;
        let id = format!("r{}", self.id);
        self.raw(json!({"v":1,"type":"request","id":id,"command":command,"params":params}))
            .await;
        id
    }
    async fn raw(&mut self, value: Value) {
        self.write
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn next(&mut self) -> Value {
        serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap()
    }
    async fn request(&mut self, command: &str, params: Value) -> Vec<Value> {
        let id = self.send(command, params).await;
        let mut frames = Vec::new();
        loop {
            let v = self.next().await;
            assert_eq!(v["id"], id);
            assert_eq!(v["seq"], frames.len());
            let terminal = v["type"] != "event";
            frames.push(v);
            if terminal {
                node_validate(command, &frames);
                return frames;
            }
        }
    }
    async fn value(&mut self, command: &str) -> Value {
        let frames = self.request(command, json!({})).await;
        assert_eq!(frames.last().unwrap()["type"], "result", "{frames:?}");
        frames.last().unwrap()["data"].clone()
    }
    async fn close(mut self) {
        self.write.shutdown().await.unwrap();
        assert!(self.lines.next_line().await.unwrap().is_none());
        self.server.await.unwrap().unwrap();
    }
}
fn summary(frames: &[Value]) -> &Value {
    assert_eq!(frames.last().unwrap()["type"], "result", "{frames:?}");
    &frames.last().unwrap()["data"]
}

fn node_validate(command: &str, frames: &[Value]) {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut child = Command::new("node")
        .args([
            "--input-type=module",
            "-e",
            r#"
        import { parseEvent, terminalEvent } from './electron/adapters.mjs';
        import { parseResponse } from './electron/validation.mjs';
        const input = []; for await (const c of process.stdin) input.push(c);
        const { command, frames } = JSON.parse(Buffer.concat(input));
        for (const f of frames) {
          if (f.type === 'event') parseEvent(command, f.event, f.data);
          if (f.type === 'result') {
            if (command === 'index_papers_stream') terminalEvent(command, f.data);
            else parseResponse(command, f.data);
          }
        }
    "#,
        ])
        .current_dir(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&json!({"command":command,"frames":frames})).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Electron contract: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn indexing_skip_change_force_empty_failure_restart_and_reset_containment() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    text(&paths, "a", "A useful paper about native search.");
    std::fs::write(
        paths.papers.join("b.pdf"),
        pdf(vec![Page::Blank, Page::Image]),
    )
    .unwrap();
    std::fs::write(paths.papers.join("c.pdf"), pdf(vec![Page::Corrupt])).unwrap();
    std::fs::write(
        paths.history_database.clone(),
        b"canonical history sentinel",
    )
    .unwrap();
    let pdf_before = std::fs::read(paths.papers.join("a.pdf")).unwrap();
    let fake = Arc::new(Fake::default());
    let mut client = Client::new(service(&paths, fake.clone(), 2).await);
    let frames = client.request("index_papers_stream", json!({})).await;
    assert_eq!(
        summary(&frames),
        &json!({"completed":3,"total":3,"indexed":1,"empty":1,"failed":1,"skipped":0})
    );
    assert_eq!(frames.iter().filter(|f| f["event"] == "started").count(), 3);
    assert_eq!(
        frames.iter().filter(|f| f["event"] == "progress").count(),
        3
    );
    let status = client.value("paper_statuses").await;
    assert_eq!(status["readiness"], "ready");
    assert_eq!(status["statuses"][0]["status"], "indexed");
    assert_eq!(status["statuses"][1]["status"], "empty");
    assert_eq!(status["statuses"][2]["status"], "not_indexed");
    assert_eq!(status["statuses"][2]["attempt"]["status"], "failed");
    assert_eq!(client.value("bm25_status").await["paper_ids"], json!(["a"]));
    let skipped = client.request("index_papers_stream", json!({})).await;
    assert_eq!(summary(&skipped)["skipped"], 2);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    fake.fail.store(true, Ordering::SeqCst);
    let failed = client
        .request("index_papers_stream", json!({"reindex":true}))
        .await;
    assert_eq!(summary(&failed)["failed"], 2);
    let status = client.value("paper_statuses").await;
    assert_eq!(status["statuses"][0]["status"], "indexed");
    assert_eq!(status["statuses"][0]["attempt"]["status"], "failed");
    text(&paths, "a", "Changed source for stale coverage.");
    assert_eq!(
        client.value("paper_statuses").await["statuses"][0]["status"],
        "stale"
    );
    assert_eq!(client.value("bm25_status").await["paper_ids"], json!([]));
    fake.fail.store(false, Ordering::SeqCst);
    assert_eq!(
        summary(&client.request("index_papers_stream", json!({})).await)["indexed"],
        1
    );
    let version = client.value("paper_statuses").await["statuses"][0]["version_id"].clone();
    assert_eq!(
        summary(
            &client
                .request("index_papers_stream", json!({"reindex":true}))
                .await
        )["indexed"],
        1
    );
    assert_ne!(
        client.value("paper_statuses").await["statuses"][0]["version_id"],
        version
    );
    client.close().await;
    let mut client = Client::new(service(&paths, fake.clone(), 2).await);
    let status = client.value("paper_statuses").await;
    assert_eq!(status["statuses"][0]["status"], "indexed");
    assert!(
        status["statuses"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["attempt"].is_null())
    );
    let before_reset = std::fs::read(paths.papers.join("a.pdf")).unwrap();
    let reset = client.value("reset_all_indices").await;
    assert_eq!(reset["reset"]["graph"], true);
    assert_eq!(
        client.value("paper_statuses").await["statuses"][0]["status"],
        "not_indexed"
    );
    assert_eq!(client.value("bm25_status").await["chunk_count"], 0);
    assert_eq!(
        std::fs::read(&paths.history_database).unwrap(),
        b"canonical history sentinel"
    );
    assert_eq!(
        std::fs::read(paths.papers.join("a.pdf")).unwrap(),
        before_reset
    );
    assert_ne!(pdf_before, before_reset);
    assert!(fake.peak.load(Ordering::SeqCst) <= 2);
    client.close().await;
}

#[tokio::test]
async fn cancellation_joins_providers_and_serializes_competing_index_and_reset() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    text(&paths, "a", "One.");
    text(&paths, "b", "Two.");
    let fake = Arc::new(Fake::default());
    fake.hold.store(true, Ordering::SeqCst);
    let mut client = Client::new(service(&paths, fake.clone(), 1).await);
    let id = client.send("index_papers_stream", json!({})).await;
    assert_eq!(client.next().await["event"], "start");
    assert_eq!(client.next().await["event"], "started");
    fake.entered.notified().await;
    for cmd in ["reset_all_indices", "index_papers"] {
        client.send(cmd, json!({})).await;
        assert_eq!(client.next().await["error"]["code"], "busy");
    }
    client.raw(json!({"v":1,"type":"cancel","id":id})).await;
    assert_eq!(client.next().await["accepted"], true);
    // Acknowledgment is not terminal; owned native/provider work must really exit.
    assert!(
        tokio::time::timeout(Duration::from_millis(30), client.lines.next_line())
            .await
            .is_err()
    );
    fake.release.notify_one();
    assert_eq!(client.next().await["type"], "cancelled");
    assert_eq!(fake.active.load(Ordering::SeqCst), 0);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let statuses = client.value("paper_statuses").await;
    assert_eq!(statuses["statuses"][0]["status"], "not_indexed");
    assert_eq!(statuses["statuses"][0]["attempt"]["status"], "interrupted");
    fake.hold.store(false, Ordering::SeqCst);
    assert_eq!(
        summary(&client.request("index_papers_stream", json!({})).await)["indexed"],
        2
    );
    client.close().await;
}

#[tokio::test]
async fn cancelled_later_paper_preserves_earlier_publication_on_reopen() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    text(&paths, "a", "First.");
    text(&paths, "b", "Second.");
    let fake = Arc::new(Fake::default());
    fake.hold_after.store(2, Ordering::SeqCst);
    let mut client = Client::new(service(&paths, fake.clone(), 1).await);
    let id = client.send("index_papers_stream", json!({})).await;
    let mut published = false;
    loop {
        let frame = client.next().await;
        if frame["event"] == "progress" && frame["data"]["paper_id"] == "a" {
            assert_eq!(frame["data"]["status"], "indexed");
            published = true;
        }
        if frame["event"] == "started" && frame["data"]["paper_id"] == "b" {
            break;
        }
    }
    assert!(published);
    while fake.calls.load(Ordering::SeqCst) < 2 {
        fake.entered.notified().await;
    }
    client.raw(json!({"v":1,"type":"cancel","id":id})).await;
    assert_eq!(client.next().await["accepted"], true);
    fake.release.notify_one();
    assert_eq!(client.next().await["type"], "cancelled");
    client.close().await;
    let mut client = Client::new(service(&paths, Arc::new(Fake::default()), 1).await);
    let statuses = client.value("paper_statuses").await;
    assert_eq!(statuses["statuses"][0]["status"], "indexed");
    assert_eq!(statuses["statuses"][1]["status"], "not_indexed");
    client.close().await;
}

#[tokio::test]
async fn panic_and_changed_source_do_not_publish_and_retries_require_explicit_request() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    text(&paths, "a", "First.");
    let fake = Arc::new(Fake::default());
    fake.panic.store(true, Ordering::SeqCst);
    let mut client = Client::new(service(&paths, fake.clone(), 1).await);
    let frames = client.request("index_papers_stream", json!({})).await;
    assert_eq!(frames.last().unwrap()["error"]["code"], "internal");
    assert_eq!(fake.active.load(Ordering::SeqCst), 0);
    assert_eq!(
        client.value("paper_statuses").await["statuses"][0]["status"],
        "not_indexed"
    );
    fake.panic.store(false, Ordering::SeqCst);
    fake.hold.store(true, Ordering::SeqCst);
    client.send("index_papers_stream", json!({})).await;
    client.next().await;
    client.next().await;
    // Consume any earlier notification, then wait for the second call's admission.
    while fake.calls.load(Ordering::SeqCst) < 2 {
        fake.entered.notified().await;
    }
    text(&paths, "a", "Replaced while provider was preparing.");
    fake.release.notify_one();
    let progress = client.next().await;
    assert_eq!(progress["data"]["status"], "failed");
    assert_eq!(client.next().await["data"]["failed"], 1);
    assert_eq!(
        client.value("paper_statuses").await["statuses"][0]["status"],
        "not_indexed"
    );
    client.close().await;
}

#[tokio::test]
async fn strict_requests_and_blocked_readiness_never_enable_mutation() {
    for reason in ["rebuild_required", "recovery_required", "unavailable"] {
        let mut registry = Registry::foundation();
        install_commands(&mut registry, Err(reason));
        let mut client = Client::registry(registry);
        assert_eq!(
            client.value("paper_statuses").await,
            json!({"readiness":reason,"statuses":[]})
        );
        assert_eq!(client.value("bm25_status").await["built"], false);
        assert_eq!(
            client.request("reset_all_indices", json!({})).await[0]["error"]["code"],
            "unavailable"
        );
        assert_eq!(
            client
                .request("paper_statuses", json!({"extra":true}))
                .await[0]["error"]["code"],
            "invalid_params"
        );
        client.close().await;
    }
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let fake = Arc::new(Fake::default());
    let mut client = Client::new(service(&paths, fake.clone(), 1).await);
    for params in [json!({"reindex":"true"}), json!({"path":"forbidden"})] {
        assert_eq!(
            client.request("index_papers", params).await[0]["error"]["code"],
            "invalid_params"
        );
    }
    assert_eq!(client.value("index_papers").await["summary"]["total"], 0);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    client.close().await;
}

#[tokio::test]
async fn graph_publication_fault_compensates_or_blocks_until_provider_free_recovery() {
    for cleanup_fails in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        text(&paths, "a", "First.");
        let fake = Arc::new(Fake::default());
        let mut client = Client::new(service(&paths, fake.clone(), 1).await);
        client.value("index_papers").await;
        let prior = client.value("paper_statuses").await["statuses"][0]["version_id"].clone();
        let db = sqlite::open(&paths.index_state_database).unwrap();
        let extra = if cleanup_fails {
            " OR instr(NEW.payload,'\"phase\":\"Compensating\"')>0"
        } else {
            ""
        };
        db.execute(format!("CREATE TRIGGER inject_graph_fault BEFORE UPDATE ON operations WHEN instr(NEW.payload,'\"phase\":\"GraphApplied\"')>0{extra} BEGIN SELECT RAISE(FAIL,'synthetic graph publication fault'); END")).unwrap();
        let frames = client
            .request("index_papers_stream", json!({"reindex":true}))
            .await;
        assert_eq!(frames.last().unwrap()["type"], "error");
        assert!(!frames.iter().any(|f| f["event"] == "progress"));
        let status = client.value("paper_statuses").await;
        if cleanup_fails {
            assert_eq!(
                status,
                json!({"readiness":"recovery_required","statuses":[]})
            );
            assert_eq!(client.value("bm25_status").await["built"], false);
            assert_eq!(
                client.request("reset_all_indices", json!({})).await[0]["type"],
                "error"
            );
        } else {
            assert_eq!(status["readiness"], "ready");
            assert_eq!(status["statuses"][0]["version_id"], prior);
            let mut statement = db.prepare("SELECT payload FROM index_meta").unwrap();
            statement.next().unwrap();
            let meta: Value =
                serde_json::from_str(&statement.read::<String, _>(0).unwrap()).unwrap();
            assert_eq!(meta["body"]["fence"]["generation"], 3);
        }
        db.execute("DROP TRIGGER inject_graph_fault").unwrap();
        drop(db);
        client.close().await;
        let before = fake.calls.load(Ordering::SeqCst);
        let mut client = Client::new(service(&paths, fake.clone(), 1).await);
        let status = client.value("paper_statuses").await;
        assert_eq!(status["statuses"][0]["status"], "indexed");
        assert_eq!(status["statuses"][0]["attempt"], Value::Null);
        assert_eq!(fake.calls.load(Ordering::SeqCst), before);
        if !cleanup_fails {
            assert_eq!(status["statuses"][0]["version_id"], prior);
        }
        client.close().await;
    }
}

#[tokio::test]
async fn reset_failure_stays_blocked_and_restart_finishes_only_derived_reset() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    text(&paths, "a", "First.");
    let original = std::fs::read(paths.papers.join("a.pdf")).unwrap();
    std::fs::write(&paths.history_database, b"preserved history").unwrap();
    let fake = Arc::new(Fake::default());
    let mut client = Client::new(service(&paths, fake.clone(), 1).await);
    client.value("index_papers").await;
    let db = sqlite::open(&paths.index_state_database).unwrap();
    db.execute("CREATE TRIGGER inject_reset_fault BEFORE UPDATE ON operations WHEN instr(NEW.payload,'\"phase\":\"ResetGraph\"')>0 BEGIN SELECT RAISE(FAIL,'synthetic reset publication fault'); END").unwrap();
    assert_eq!(
        client.request("reset_all_indices", json!({})).await[0]["type"],
        "error"
    );
    assert_eq!(
        client.value("paper_statuses").await["readiness"],
        "recovery_required"
    );
    db.execute("DROP TRIGGER inject_reset_fault").unwrap();
    drop(db);
    client.close().await;
    let mut client = Client::new(service(&paths, fake.clone(), 1).await);
    assert_eq!(
        client.value("paper_statuses").await["statuses"][0]["status"],
        "not_indexed"
    );
    assert_eq!(client.value("bm25_status").await["chunk_count"], 0);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(paths.papers.join("a.pdf")).unwrap(), original);
    assert_eq!(
        std::fs::read(&paths.history_database).unwrap(),
        b"preserved history"
    );
    client.close().await;
}

#[tokio::test]
async fn explicit_production_composition_is_provider_free_and_missing_config_is_unavailable() {
    use research_backend::composition::native_registry;
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    text(&paths, "a", "First.");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let env = std::collections::HashMap::from([
        ("CAPSTONE_NATIVE_INDEXING", "1".into()),
        ("CAPSTONE_EMBED_MODEL", "fake-embedding".into()),
        ("CAPSTONE_CHAT_MODEL", "fake-metadata".into()),
        ("CAPSTONE_EMBED_BASE_URL", endpoint.clone()),
        ("CAPSTONE_CHAT_BASE_URL", endpoint),
        ("CAPSTONE_EMBED_DIMENSIONS", "2".into()),
        ("OPENAI_API_KEY", "fixture-not-a-real-credential".into()),
        (
            "CAPSTONE_APP_DATA_ROOT",
            paths.data_root.to_str().unwrap().into(),
        ),
        (
            "CAPSTONE_RESOURCES_DIR",
            paths.resources.to_str().unwrap().into(),
        ),
        ("CAPSTONE_PAPERS_DIR", paths.papers.to_str().unwrap().into()),
    ]);
    for missing in [
        "CAPSTONE_PAPERS_DIR",
        "CAPSTONE_EMBED_MODEL",
        "CAPSTONE_CHAT_MODEL",
        "CAPSTONE_EMBED_BASE_URL",
        "CAPSTONE_CHAT_BASE_URL",
        "CAPSTONE_EMBED_DIMENSIONS",
        "OPENAI_API_KEY",
    ] {
        let registry = native_registry(|key| {
            if key == missing {
                None
            } else {
                env.get(key).cloned()
            }
        })
        .await;
        let mut client = Client::registry(registry);
        assert_eq!(
            client.value("paper_statuses").await["readiness"],
            "unavailable"
        );
        assert_eq!(
            client.request("index_papers", json!({})).await[0]["error"]["code"],
            "unavailable"
        );
        client.close().await;
        assert!(
            std::fs::read_dir(&paths.data_root)
                .unwrap()
                .next()
                .is_none()
        );
    }
    for (key, invalid) in [
        ("CAPSTONE_EMBED_DIMENSIONS", "0"),
        ("CAPSTONE_EMBED_DIMENSIONS", "bad"),
        ("CAPSTONE_INDEX_MAX_WORKERS", "33"),
        ("CAPSTONE_CHAT_BASE_URL", "file:///invalid"),
        ("CAPSTONE_EMBED_MODEL", ""),
    ] {
        let registry = native_registry(|name| {
            if name == key {
                Some(invalid.into())
            } else {
                env.get(name).cloned()
            }
        })
        .await;
        let mut client = Client::registry(registry);
        assert_eq!(
            client.value("paper_statuses").await["readiness"],
            "unavailable"
        );
        client.close().await;
        assert!(
            std::fs::read_dir(&paths.data_root)
                .unwrap()
                .next()
                .is_none()
        );
    }
    let registry = native_registry(|key| env.get(key).cloned()).await;
    assert_eq!(registry.health()["stage"], "native_indexing");
    let mut client = Client::registry(registry);
    assert_eq!(
        client.value("paper_statuses").await["statuses"][0]["status"],
        "not_indexed"
    );
    assert_eq!(client.value("bm25_status").await["chunk_count"], 0);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!paths.data_root.join("chat_history.sqlite3").exists());
    client.close().await;
}
