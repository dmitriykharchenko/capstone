use research_backend::{
    domain::{EvaluationCase, GraphQuery, RetrievalStrategy, SourceRecord},
    protocol::*,
    registry::Registry,
};
use serde_json::Value;

#[test]
fn shared_protocol_fixture_matches_rust_types_and_limits() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../contracts/protocol-v1.json")).unwrap();
    assert_eq!(fixture["version"], VERSION);
    assert_eq!(fixture["limits"]["frame_bytes"], MAX_FRAME_BYTES);
    assert_eq!(fixture["limits"]["active_requests"], MAX_ACTIVE_REQUESTS);
    assert_eq!(fixture["limits"]["session_requests"], MAX_SESSION_REQUESTS);
    assert_eq!(fixture["limits"]["worker_queue"], QUEUE_CAPACITY);
    for request in fixture["requests"].as_array().unwrap() {
        Input::parse(&serde_json::to_vec(request).unwrap()).unwrap();
    }
    for request in fixture["invalid_requests"].as_array().unwrap() {
        assert!(Input::parse(&serde_json::to_vec(request).unwrap()).is_err());
    }
    for output in fixture["outputs"].as_array().unwrap() {
        let typed: Output = serde_json::from_value(output.clone()).unwrap();
        assert_eq!(serde_json::to_value(typed).unwrap(), *output);
    }
    for query in fixture["graph_queries"].as_array().unwrap() {
        serde_json::from_value::<GraphQuery>(query.clone()).unwrap();
    }
    for strategy in fixture["retrieval_strategies"].as_array().unwrap() {
        serde_json::from_value::<RetrievalStrategy>(strategy.clone()).unwrap();
    }
    serde_json::from_value::<SourceRecord>(fixture["source"].clone()).unwrap();
    serde_json::from_value::<EvaluationCase>(fixture["evaluation_case"].clone()).unwrap();
}

#[test]
fn inventory_contains_all_ui_commands_and_non_ui_capabilities() {
    let health = Registry::foundation().health();
    let commands = health["commands"].as_array().unwrap();
    let expected = [
        "health",
        "list_papers",
        "get_paper",
        "read_pdf",
        "chat",
        "chat_stream",
        "list_sessions",
        "get_messages",
        "search",
        "search_papers",
        "graph_neighborhood",
        "index_papers",
        "index_papers_stream",
        "paper_statuses",
        "bm25_status",
        "reset_all_indices",
        "evaluate_stream",
        "graph_query",
        "retrieve_chunks",
    ];
    assert_eq!(commands.len(), expected.len());
    for name in expected {
        let entry = commands.iter().find(|c| c["command"] == name).unwrap();
        assert_eq!(entry["available"], name == "health");
    }
    assert_eq!(health["chat_mode"], "unavailable");
    assert_eq!(health["search_enabled"], false);
}

#[test]
fn invalid_envelopes_are_rejected_without_reflecting_input() {
    for bytes in [
        b"not-json".as_slice(),
        b"[]",
        b"null",
        b"\xff",
        br#"{"v":1,"type":"cancel","id":"bad\nline"}"#,
        br#"{"v":1,"type":"request","id":"x","id":"y","command":"health","params":{}}"#,
    ] {
        assert_eq!(Input::parse(bytes).unwrap_err(), ErrorCode::InvalidRequest);
    }
}

#[test]
fn every_command_has_a_transport_fixture_for_its_downstream_owner() {
    let requests: Vec<Value> =
        serde_json::from_str(include_str!("../../contracts/command-requests.json")).unwrap();
    let health = Registry::foundation().health();
    let commands = health["commands"].as_array().unwrap();
    assert_eq!(requests.len(), commands.len());
    for command in commands {
        let matches: Vec<_> = requests
            .iter()
            .filter(|r| r["command"] == command["command"])
            .collect();
        assert_eq!(matches.len(), 1);
        Input::parse(&serde_json::to_vec(matches[0]).unwrap()).unwrap();
    }
}
