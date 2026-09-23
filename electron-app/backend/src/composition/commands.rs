//! Final desktop command wiring; no independent stores or fallback providers.
use crate::{
    domain::GraphQuery,
    evaluations::{EvaluationRequest, NativeEvaluationService},
    indexing::CoordinatedIndex,
    protocol::ErrorCode,
    registry::{BackendFuture, CommandHandler, Registry},
    retrieval::{NativeRetriever, SearchRequest},
    runtime::RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) fn install(
    registry: &mut Registry,
    index: CoordinatedIndex,
    retriever: NativeRetriever,
    evaluations: Option<NativeEvaluationService>,
) {
    for (command, owner) in [
        ("search_papers", "retrieval"),
        ("retrieve_chunks", "retrieval"),
        ("graph_neighborhood", "graph"),
        ("graph_query", "graph"),
        ("evaluate_stream", "evaluations"),
    ] {
        if command == "evaluate_stream" && evaluations.is_none() {
            continue;
        }
        registry
            .install(
                command,
                owner,
                Arc::new(Handler {
                    command,
                    index: index.clone(),
                    retriever: retriever.clone(),
                    evaluations: evaluations.clone(),
                }),
            )
            .expect("checked-in desktop command ownership slots");
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaperSearch {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    8
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Neighborhood {
    node_id: String,
    #[serde(default = "default_depth")]
    max_depth: u32,
}
fn default_depth() -> u32 {
    1
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ErrorCode> {
    serde_json::from_value(value).map_err(|_| ErrorCode::InvalidParams)
}
#[derive(Clone)]
struct Handler {
    command: &'static str,
    index: CoordinatedIndex,
    retriever: NativeRetriever,
    evaluations: Option<NativeEvaluationService>,
}
impl CommandHandler for Handler {
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        let this = self.clone();
        Box::pin(async move {
            let token = context.cancellation();
            token.check()?;
            if matches!(this.command, "graph_query" | "graph_neighborhood") {
                let query = if this.command == "graph_query" {
                    parse::<GraphQuery>(params)?
                } else {
                    let request = parse::<Neighborhood>(params)?;
                    GraphQuery::Neighborhood {
                        node_id: request.node_id,
                        max_depth: request.max_depth,
                    }
                };
                return this.index.query_graph(query, token).await;
            }
            let _guard = this
                .index
                .gate()
                .read()
                .await
                .map_err(|_| ErrorCode::Unavailable)?;
            this.index
                .verify_read()
                .await
                .map_err(|_| ErrorCode::Unavailable)?;
            token.check()?;
            match this.command {
                "search_papers" => {
                    let request = parse::<PaperSearch>(params)?;
                    if request.query.trim().is_empty() || !(1..=100).contains(&request.limit) {
                        return Err(ErrorCode::InvalidParams);
                    }
                    let matches = this
                        .retriever
                        .search_papers(request.query, request.limit, token)
                        .await?;
                    Ok(json!({"enabled":true,"matches":matches}))
                }
                "retrieve_chunks" => {
                    let request = parse::<SearchRequest>(params)?;
                    if !(1..=100).contains(&request.limit) {
                        return Err(ErrorCode::InvalidParams);
                    }
                    let chunks = this.retriever.retrieve_typed(request, token).await?;
                    Ok(json!({"enabled":true,"chunks":chunks}))
                }
                "evaluate_stream" => {
                    this.evaluations
                        .ok_or(ErrorCode::Unavailable)?
                        .run_request(EvaluationRequest::from_value(params)?, context)
                        .await
                }
                _ => Err(ErrorCode::Unavailable),
            }
        })
    }
}
