use crate::{
    registry::{BackendFuture, CommandHandler, Registry},
    runtime::{CancellationMode, OperationGroup, RequestContext, serve},
};
use serde_json::{Value, json};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Handler<F>(Mutex<Option<F>>);
impl<F, Fut> CommandHandler for Handler<F>
where
    F: FnOnce(OperationGroup) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn cancellation_mode(&self) -> CancellationMode {
        CancellationMode::Cooperative
    }
    fn call(&self, _: Value, context: RequestContext) -> BackendFuture<Value> {
        let work = self.0.lock().unwrap().take().unwrap();
        Box::pin(async move {
            let group = context.operation_group()?;
            work(group.clone()).await;
            let _permit = group.seal_completion()?;
            Ok(json!({"done":true}))
        })
    }
}

/// Real request-owned OperationGroup; no unlinked mutation token test shortcut.
/// A handler assertion/panic is verified externally as a failed terminal.
pub async fn with_group<F, Fut>(work: F)
where
    F: FnOnce(OperationGroup) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut registry = Registry::foundation();
    registry
        .install(
            "index_papers_stream",
            "indexing",
            Arc::new(Handler(Mutex::new(Some(work)))),
        )
        .unwrap();
    let (client, server) = tokio::io::duplex(8192);
    let (r, w) = tokio::io::split(server);
    let task = tokio::spawn(serve(r, w, registry));
    let (r, mut w) = tokio::io::split(client);
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"v\":1,\"type\":\"request\",\"id\":\"snapshot\",\"command\":\"index_papers_stream\",\"params\":{}}\n").await.unwrap();
    let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["type"], "result", "handler failed: {value}");
    assert_eq!(value["data"]["done"], true);
    w.shutdown().await.unwrap();
    assert!(lines.next_line().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
