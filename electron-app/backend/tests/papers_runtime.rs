use research_backend::{
    papers::*,
    protocol::MAX_FRAME_BYTES,
    registry::{BackendFuture, CommandHandler, Registry},
    runtime::{Cancellation, RequestContext, serve},
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
mod papers_fixtures;
use papers_fixtures::{Page, encrypted_pdf, pdf};

struct Client {
    writer: WriteHalf<DuplexStream>,
    reader: Lines<BufReader<ReadHalf<DuplexStream>>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Client {
    fn new(repository: NativePaperRepository) -> Self {
        let mut registry = Registry::foundation();
        for (name, command) in [
            ("list_papers", PaperCommand::List),
            ("get_paper", PaperCommand::Get),
            ("read_pdf", PaperCommand::ReadPdf),
        ] {
            registry
                .install(
                    name,
                    "papers",
                    Arc::new(PaperCommandHandler::new(repository.clone(), command)),
                )
                .unwrap();
        }
        Self::registry(registry)
    }

    fn registry(registry: Registry) -> Self {
        let (client, server) = tokio::io::duplex(32 * 1024);
        let (input, output) = tokio::io::split(server);
        let task = tokio::spawn(serve(input, output, registry));
        let (reader, writer) = tokio::io::split(client);
        Self {
            writer,
            reader: BufReader::new(reader).lines(),
            task,
        }
    }

    async fn send(&mut self, message: Value) {
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await.unwrap();
    }

    async fn request(&mut self, id: &str, command: &str, params: Value) {
        self.send(json!({"v":1,"type":"request","id":id,"command":command,"params":params}))
            .await;
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(5), self.reader.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(line.len() <= MAX_FRAME_BYTES);
        serde_json::from_str(&line).unwrap()
    }

    async fn close(mut self) {
        self.writer.shutdown().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), self.reader.next_line())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        self.task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn runtime_streams_exact_offsets_bytes_and_one_terminal_with_typed_metadata() {
    let root = tempfile::tempdir().unwrap();
    let bytes = pdf(vec![Page::Text(vec![vec![b'x'; PDF_CHUNK_BYTES * 3 + 7]])]);
    fs::write(root.path().join("DBLP_conf_Test.pdf"), &bytes).unwrap();
    let mut client = Client::new(NativePaperRepository::new(root.path()).unwrap());
    client.request("list", "list_papers", json!({})).await;
    let list = client.next().await;
    assert_eq!(
        list["data"],
        json!([{"id":"DBLP_conf_Test","filename":"DBLP_conf_Test.pdf","title":"Test","size_bytes":bytes.len()}])
    );
    client
        .request("get", "get_paper", json!({"paper_id":"DBLP_conf_Test"}))
        .await;
    assert_eq!(client.next().await["data"], list["data"][0]);
    client
        .request("pdf", "read_pdf", json!({"paper_id":"DBLP_conf_Test"}))
        .await;
    let mut assembled = Vec::new();
    for sequence in 0..4 {
        let message = client.next().await;
        assert_eq!(message["id"], "pdf");
        assert_eq!(message["seq"], sequence);
        assert_eq!(message["type"], "event");
        assert_eq!(message["event"], "pdf_chunk");
        let chunk: PdfChunk = serde_json::from_value(message["data"].clone()).unwrap();
        assert_eq!(chunk.offset as usize, assembled.len());
        assert!(!chunk.bytes.is_empty() && chunk.bytes.len() <= PDF_CHUNK_BYTES);
        assembled.extend_from_slice(&chunk.bytes);
    }
    let terminal = client.next().await;
    assert_eq!(terminal["seq"], 4);
    assert_eq!(terminal["type"], "result");
    assert_eq!(
        terminal["data"],
        json!({"id":"DBLP_conf_Test","filename":"DBLP_conf_Test.pdf","mime_type":"application/pdf","total_bytes":bytes.len()})
    );
    assert_eq!(assembled, bytes);
    client.send(json!({"v":1,"type":"cancel","id":"pdf"})).await;
    assert_eq!(client.next().await["accepted"], false);
    client.close().await;
}

#[tokio::test]
async fn invalid_typed_requests_and_preflight_failures_emit_no_partial_data() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("empty.pdf"), []).unwrap();
    fs::write(root.path().join("corrupt.pdf"), b"not a PDF").unwrap();
    let valid = pdf(vec![Page::Blank]);
    fs::write(root.path().join("truncated.pdf"), &valid[..valid.len() / 2]).unwrap();
    let mut forged = pdf_extract::Document::load_mem(&valid).unwrap();
    let invalid_encryption = forged.add_object(pdf_extract::dictionary! {});
    forged.trailer.set("Encrypt", invalid_encryption);
    let mut forged_bytes = Vec::new();
    forged.save_to(&mut forged_bytes).unwrap();
    fs::write(root.path().join("forged.pdf"), forged_bytes).unwrap();
    fs::File::create(root.path().join("large.pdf"))
        .unwrap()
        .set_len(MAX_PDF_BYTES + 1)
        .unwrap();
    let mut client = Client::new(NativePaperRepository::new(root.path()).unwrap());
    for (index, params) in [
        json!({}),
        json!({"paper_id":1}),
        json!({"paper_id":"../secret"}),
        json!({"paper_id":"missing"}),
        json!({"paper_id":"empty"}),
        json!({"paper_id":"large"}),
        json!({"paper_id":"corrupt"}),
        json!({"paper_id":"truncated"}),
        json!({"paper_id":"forged"}),
        json!({"paper_id":"empty","path":"/private"}),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("bad-{index}");
        client.request(&id, "read_pdf", params).await;
        let result = client.next().await;
        assert_eq!(result["id"], id);
        assert_eq!(result["seq"], 0);
        assert_eq!(result["type"], "error");
        assert_eq!(result["error"]["code"], "invalid_params");
        assert_eq!(result["error"]["message"], "Invalid command parameters.");
    }
    client
        .request("bad-list", "list_papers", json!({"extra":true}))
        .await;
    assert_eq!(client.next().await["error"]["code"], "invalid_params");
    client.close().await;
}

#[tokio::test]
async fn cancelling_real_transfer_discards_late_chunks_and_remains_responsive() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("large.pdf"),
        pdf(vec![Page::Text(vec![vec![b'x'; 8 * 1024 * 1024]])]),
    )
    .unwrap();
    let mut client = Client::new(NativePaperRepository::new(root.path()).unwrap());
    client
        .request("pdf", "read_pdf", json!({"paper_id":"large"}))
        .await;
    assert_eq!(client.next().await["event"], "pdf_chunk");
    client.send(json!({"v":1,"type":"cancel","id":"pdf"})).await;
    let mut sequence = 1;
    let mut acknowledged = false;
    loop {
        let message = client.next().await;
        if message["type"] == "cancel_ack" {
            assert_eq!(message["accepted"], true);
            acknowledged = true;
            continue;
        }
        assert_eq!(message["seq"], sequence);
        if message["type"] == "cancelled" {
            break;
        }
        assert_eq!(message["type"], "event");
        sequence += 1;
    }
    assert!(acknowledged);
    client.request("health", "health", json!({})).await;
    let health = client.next().await;
    assert_eq!(health["id"], "health");
    assert_eq!(health["type"], "result");
    client.close().await;
}

#[tokio::test]
async fn preview_transfers_blank_image_nonextractable_and_password_protected_bytes_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::new(NativePaperRepository::new(root.path()).unwrap());
    for (index, bytes) in [
        pdf(vec![Page::Blank]),
        pdf(vec![Page::Image]),
        pdf(vec![Page::Corrupt]),
        encrypted_pdf(false),
        encrypted_pdf(true),
    ]
    .into_iter()
    .enumerate()
    {
        fs::write(root.path().join("preview.pdf"), &bytes).unwrap();
        let id = format!("preview-{index}");
        client
            .request(&id, "read_pdf", json!({"paper_id":"preview"}))
            .await;
        let mut assembled = Vec::new();
        loop {
            let message = client.next().await;
            if message["type"] == "result" {
                assert_eq!(message["data"]["total_bytes"], bytes.len());
                break;
            }
            assert_eq!(message["type"], "event");
            let chunk: PdfChunk = serde_json::from_value(message["data"].clone()).unwrap();
            assert_eq!(chunk.offset as usize, assembled.len());
            assembled.extend(chunk.bytes);
        }
        assert!(assembled == bytes, "preview must transfer identical bytes");
    }
    client.close().await;
}

struct CaptureCancellation(Arc<OnceLock<Cancellation>>);
impl CommandHandler for CaptureCancellation {
    fn call(&self, _: Value, context: RequestContext) -> BackendFuture<Value> {
        let token = self.0.clone();
        Box::pin(async move {
            assert!(token.set(context.cancellation()).is_ok());
            context.emit("ready", json!({})).await?;
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn actual_runtime_cancellation_is_checked_before_read_and_extract() {
    let token = Arc::new(OnceLock::new());
    let mut registry = Registry::foundation();
    registry
        .install(
            "read_pdf",
            "papers",
            Arc::new(CaptureCancellation(token.clone())),
        )
        .unwrap();
    let mut client = Client::registry(registry);
    client.request("capture", "read_pdf", json!({})).await;
    assert_eq!(client.next().await["event"], "ready");
    client
        .send(json!({"v":1,"type":"cancel","id":"capture"}))
        .await;
    assert_eq!(client.next().await["accepted"], true);
    assert_eq!(client.next().await["type"], "cancelled");
    let cancellation = token.get().unwrap();
    let root = tempfile::tempdir().unwrap();
    let repository = NativePaperRepository::new(root.path()).unwrap();
    assert_eq!(
        repository.read_pdf_bytes("missing", cancellation),
        Err(PaperError::Cancelled)
    );
    assert_eq!(
        extract_pdf_pages(b"invalid", cancellation),
        Err(PaperError::Cancelled)
    );
    client.close().await;
}
