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
            ("read_pdf_range", PaperCommand::ReadPdfRange),
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
        while let Some(line) = tokio::time::timeout(Duration::from_secs(5), self.reader.next_line())
            .await
            .unwrap()
            .unwrap()
        {
            let value: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(value["type"], "cancelled");
        }
        self.task.await.unwrap().unwrap();
    }
    async fn cancel_pdf(&mut self, id: &str) {
        self.send(json!({"v":1,"type":"cancel","id":id})).await;
        assert_eq!(self.next().await["accepted"], true);
        assert_eq!(self.next().await["type"], "cancelled");
    }
}

#[tokio::test]
async fn runtime_leases_metadata_and_reads_only_requested_bounded_ranges() {
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
    let opened = client.next().await;
    assert_eq!(opened["event"], "pdf_open");
    assert_eq!(opened["data"]["total_bytes"], bytes.len());
    let preview_id = opened["data"]["preview_id"].clone();
    assert!(uuid::Uuid::parse_str(preview_id.as_str().unwrap()).is_ok());
    assert!(opened["data"].get("bytes").is_none());
    for sequence in 0..4 {
        let begin = sequence * PDF_CHUNK_BYTES;
        client.request(&format!("pdf:{}", sequence + 1), "read_pdf_range", json!({"preview_id":preview_id,"begin":begin,"end":bytes.len().min(begin + PDF_CHUNK_BYTES)})).await;
        let message = client.next().await;
        assert_eq!(message["seq"], 0);
        assert_eq!(message["type"], "result");
        let chunk: PdfChunk = serde_json::from_value(message["data"].clone()).unwrap();
        assert_eq!(chunk.offset as usize, assembled.len());
        assert!(!chunk.bytes.is_empty() && chunk.bytes.len() <= PDF_CHUNK_BYTES);
        assembled.extend_from_slice(&chunk.bytes);
    }
    assert_eq!(assembled, bytes);
    client.cancel_pdf("pdf").await;
    client
        .request(
            "pdf:5",
            "read_pdf_range",
            json!({"preview_id":preview_id,"begin":0,"end":1}),
        )
        .await;
    assert_eq!(client.next().await["error"]["code"], "pdf_closed");
    client.close().await;
}

#[tokio::test]
async fn malformed_range_shapes_close_lease_and_never_expose_internal_details() {
    for mode in ["extra", "negative", "fractional", "oversized", "past_end"] {
        let root = tempfile::tempdir().unwrap();
        let bytes = pdf(vec![Page::Blank]);
        fs::write(root.path().join("p.pdf"), &bytes).unwrap();
        let mut client = Client::new(NativePaperRepository::new(root.path()).unwrap());
        client
            .request("open", "read_pdf", json!({"paper_id":"p"}))
            .await;
        let opened = client.next().await;
        let id = opened["data"]["preview_id"].clone();
        let mut params = json!({"preview_id":id,"begin":0,"end":1});
        match mode {
            "extra" => params["path"] = json!("/private"),
            "negative" => params["begin"] = json!(-1),
            "fractional" => params["end"] = json!(0.5),
            "oversized" => params["end"] = json!(MAX_PREVIEW_RANGE + 1),
            "past_end" => params["end"] = json!(bytes.len() + 1),
            _ => unreachable!(),
        }
        client.request("pdf:1", "read_pdf_range", params).await;
        let mut ids = Vec::new();
        for _ in 0..2 {
            let result = client.next().await;
            ids.push(result["id"].as_str().unwrap().to_owned());
            assert_eq!(result["error"]["code"], "pdf_invalid");
            assert_eq!(result["error"]["message"], "PDF is invalid or unsupported.");
        }
        ids.sort();
        assert_eq!(ids, ["open", "pdf:1"]);
        client
            .request(
                "pdf:2",
                "read_pdf_range",
                json!({"preview_id":id,"begin":0,"end":1}),
            )
            .await;
        assert_eq!(client.next().await["error"]["code"], "pdf_closed");
        client.close().await;
    }
}

#[tokio::test]
async fn invalid_typed_requests_and_preflight_failures_emit_no_partial_data() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("empty.pdf"), []).unwrap();
    fs::write(root.path().join("corrupt.pdf"), b"not a PDF").unwrap();
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
        assert!(
            ["invalid_params", "pdf_invalid"].contains(&result["error"]["code"].as_str().unwrap())
        );
        assert!(
            [
                "Invalid command parameters.",
                "PDF is invalid or unsupported."
            ]
            .contains(&result["error"]["message"].as_str().unwrap())
        );
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
    assert_eq!(client.next().await["event"], "pdf_open");
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
        let metadata = client.next().await;
        assert_eq!(metadata["event"], "pdf_open");
        for begin in (0..bytes.len()).step_by(PDF_CHUNK_BYTES) {
            client.request(&format!("range-{index}-{begin}"), "read_pdf_range", json!({"preview_id":metadata["data"]["preview_id"],"begin":begin,"end":bytes.len().min(begin + PDF_CHUNK_BYTES)})).await;
            let message = client.next().await;
            assert_eq!(message["type"], "result");
            let chunk: PdfChunk = serde_json::from_value(message["data"].clone()).unwrap();
            assert_eq!(chunk.offset as usize, assembled.len());
            assembled.extend(chunk.bytes);
        }
        assert!(assembled == bytes, "preview must transfer identical bytes");
        client.cancel_pdf(&id).await;
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
