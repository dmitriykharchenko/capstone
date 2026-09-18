use research_backend::{domain::GraphQuery, graph::*, runtime::Cancellation};
use serde_json::json;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).into()).collect()
}
fn replacement(id: &str, title: &str, authors: &[&str], topics: &[&str]) -> GraphOperation {
    GraphOperation::Replace(
        PreparedPaper::new(id, title, &strings(authors), &strings(topics)).unwrap(),
    )
}
async fn open(root: &std::path::Path) -> CozoGraphStore {
    CozoGraphStore::open(
        root.into(),
        root.join("graph.sqlite3"),
        Cancellation::default(),
    )
    .await
    .unwrap()
}
async fn query(store: &CozoGraphStore, q: GraphQuery) -> serde_json::Value {
    store.query(q, Cancellation::default()).await.unwrap()
}
async fn neighborhood(store: &CozoGraphStore, id: &str, depth: u32) -> serde_json::Value {
    query(
        store,
        GraphQuery::Neighborhood {
            node_id: id.into(),
            max_depth: depth,
        },
    )
    .await["neighborhood"]
        .clone()
}

#[test]
fn normalization_full_casefold_nfkc_namespace_and_stable_digest() {
    assert_eq!(
        normalize_node_name("  Graph\u{3000}Neural  Networks ").unwrap(),
        "graph neural networks"
    );
    for (a, b) in [
        ("Straße", "STRASSE"),
        ("Σςσ", "σσσ"),
        ("Ａｄａ", "Ada"),
        ("é", "e\u{301}"),
        ("ﬃ", "ffi"),
        ("Ꭰ", "ꭰ"),
        ("ᾈ", "ἀι"),
        ("İ", "i\u{307}"),
    ] {
        assert_eq!(
            stable_node_id(NodeKind::Author, a),
            stable_node_id(NodeKind::Author, b)
        );
    }
    assert_eq!(
        stable_node_id(NodeKind::Author, " Ada ").unwrap(),
        "author:fdee430d40bd57deeac186cd9790033d0f06f909a8806e7ce6e717ab7c7d5029"
    );
    assert_ne!(
        stable_node_id(NodeKind::Author, "Ada"),
        stable_node_id(NodeKind::Topic, "Ada")
    );
    assert_eq!(
        stable_node_id(NodeKind::Paper, "Ada"),
        Err(GraphError::InvalidInput)
    );
    assert_eq!(normalize_node_name(" \n "), Err(GraphError::InvalidInput));
}

#[tokio::test]
async fn all_six_query_variants_are_deterministic_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let store = open(root.path()).await;
    store
        .apply(
            vec![
                replacement("p2", "Second", &["ada", "Bob"], &["Logic", "Graphs"]),
                replacement("p1", "First", &["ada", "carol"], &["Graphs"]),
            ],
            Cancellation::default(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let store = open(root.path()).await;
    let ada = stable_node_id(NodeKind::Author, "Ada").unwrap();
    let graphs = stable_node_id(NodeKind::Topic, "Graphs").unwrap();
    let p = query(
        &store,
        GraphQuery::PaperDetails {
            paper_id: "p2".into(),
        },
    )
    .await;
    assert_eq!(p["paper"]["title"], "Second");
    assert_eq!(
        p["paper"]["authors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["ada", "Bob"]
    );
    for q in [
        GraphQuery::PapersByAuthor {
            author_id: ada.clone(),
        },
        GraphQuery::PapersByTopic {
            topic_id: graphs.clone(),
        },
    ] {
        let v = query(&store, q).await;
        assert_eq!(
            v["papers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["paper_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["p1", "p2"]
        );
    }
    let topics = query(
        &store,
        GraphQuery::TopicsByAuthor {
            author_id: ada.clone(),
        },
    )
    .await;
    assert_eq!(
        topics["topics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Graphs", "Logic"]
    );
    let authors = query(
        &store,
        GraphQuery::Coauthors {
            author_id: ada.clone(),
        },
    )
    .await;
    assert_eq!(
        authors["coauthors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Bob", "carol"]
    );
    let full = query(
        &store,
        GraphQuery::Neighborhood {
            node_id: ada,
            max_depth: 5,
        },
    )
    .await;
    assert_eq!(full["enabled"], true);
    assert_eq!(full["available"], true);
    assert_eq!(full["query"], "neighborhood");
    assert!(full.get("reason").is_none());
    assert_eq!(full["neighborhood"]["nodes"].as_array().unwrap().len(), 7);
}

#[tokio::test]
async fn replacement_dedup_orphans_shared_nodes_and_remove() {
    let root = tempfile::tempdir().unwrap();
    let store = open(root.path()).await;
    let first = replacement(
        "p1",
        "First",
        &["Ada", " Grace  Hopper ", "ada"],
        &["Graphs", "Graphs", "Databases"],
    );
    store
        .apply(vec![first.clone()], Cancellation::default())
        .await
        .unwrap();
    let original = neighborhood(&store, "p1", 5).await;
    store
        .apply(vec![first], Cancellation::default())
        .await
        .unwrap();
    assert_eq!(neighborhood(&store, "p1", 5).await, original);
    store
        .apply(
            vec![
                replacement("p2", "Second", &["Ada"], &["Graphs"]),
                replacement("p1", "Updated", &["Alan"], &["Logic"]),
            ],
            Cancellation::default(),
        )
        .await
        .unwrap();
    for (kind, name) in [
        (NodeKind::Author, "Grace Hopper"),
        (NodeKind::Topic, "Databases"),
    ] {
        assert_eq!(
            neighborhood(&store, &stable_node_id(kind, name).unwrap(), 5).await,
            json!({"nodes":[],"edges":[]})
        );
    }
    assert_eq!(
        store.status(Cancellation::default()).await.unwrap(),
        GraphStatus {
            papers: 2,
            authors: 2,
            topics: 2,
            edges: 4
        }
    );
    for _ in 0..2 {
        store
            .apply(
                vec![GraphOperation::Remove("p1".into())],
                Cancellation::default(),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        store.status(Cancellation::default()).await.unwrap(),
        GraphStatus {
            papers: 1,
            authors: 1,
            topics: 1,
            edges: 2
        }
    );
    assert!(
        !neighborhood(&store, &stable_node_id(NodeKind::Author, "Ada").unwrap(), 1).await["nodes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cycles_induced_edges_all_depths_ordering_and_missing_nodes() {
    let root = tempfile::tempdir().unwrap();
    let store = open(root.path()).await;
    store
        .apply(
            vec![
                replacement("p2", "Second", &["Ada", "Bob"], &["Graphs"]),
                replacement("p1", "First", &["Ada", "Bob"], &["Graphs"]),
            ],
            Cancellation::default(),
        )
        .await
        .unwrap();
    let ada = stable_node_id(NodeKind::Author, "Ada").unwrap();
    for depth in 0..=5 {
        let n = neighborhood(&store, &ada, depth).await;
        assert_eq!(neighborhood(&store, &ada, depth).await, n);
        assert_eq!(
            n["nodes"].as_array().unwrap().len(),
            match depth {
                0 => 1,
                1 => 3,
                _ => 5,
            }
        );
        let edges = n["edges"].as_array().unwrap();
        assert_eq!(
            edges.len(),
            match depth {
                0 => 0,
                1 => 2,
                _ => 6,
            }
        );
        let ids: std::collections::BTreeSet<_> = n["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["node_id"].as_str().unwrap())
            .collect();
        let mut keys = Vec::new();
        for e in edges {
            let kind = e["kind"].as_str().unwrap();
            let source = e["source_id"].as_str().unwrap();
            let target = e["target_id"].as_str().unwrap();
            assert!(matches!(kind, "authored_by" | "has_topic"));
            assert!(source == "p1" || source == "p2");
            assert!(ids.contains(source) && ids.contains(target));
            keys.push((kind, source, target));
        }
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
    }
    assert_eq!(
        store.status(Cancellation::default()).await.unwrap().edges,
        6
    );
    let queries = [
        (
            GraphQuery::PaperDetails {
                paper_id: "missing".into(),
            },
            "paper",
            json!(null),
        ),
        (
            GraphQuery::PapersByAuthor {
                author_id: "missing".into(),
            },
            "papers",
            json!([]),
        ),
        (
            GraphQuery::PapersByTopic {
                topic_id: "missing".into(),
            },
            "papers",
            json!([]),
        ),
        (
            GraphQuery::TopicsByAuthor {
                author_id: "missing".into(),
            },
            "topics",
            json!([]),
        ),
        (
            GraphQuery::Coauthors {
                author_id: "missing".into(),
            },
            "coauthors",
            json!([]),
        ),
        (
            GraphQuery::Neighborhood {
                node_id: "missing".into(),
                max_depth: 0,
            },
            "neighborhood",
            json!({"nodes":[],"edges":[]}),
        ),
    ];
    for (q, key, expected) in queries {
        assert_eq!(query(&store, q).await[key], expected);
    }
}

#[tokio::test]
async fn semantic_validation_and_parameter_binding_preserve_existing_graph() {
    let root = tempfile::tempdir().unwrap();
    let store = open(root.path()).await;
    let malicious = "p' ] :rm paper {paper_id} #";
    store
        .replace_paper(
            malicious.into(),
            "Quote ' \" title".into(),
            strings(&["O'Neil $rows"]),
            strings(&["Graphs \" ]"]),
            Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        query(
            &store,
            GraphQuery::PaperDetails {
                paper_id: malicious.into()
            }
        )
        .await["paper"]["title"],
        "Quote ' \" title"
    );
    for id in ["", "  \n ", "a\0b", "a\u{7}b"] {
        for q in [
            GraphQuery::PaperDetails {
                paper_id: id.into(),
            },
            GraphQuery::PapersByAuthor {
                author_id: id.into(),
            },
            GraphQuery::PapersByTopic {
                topic_id: id.into(),
            },
            GraphQuery::TopicsByAuthor {
                author_id: id.into(),
            },
            GraphQuery::Coauthors {
                author_id: id.into(),
            },
            GraphQuery::Neighborhood {
                node_id: id.into(),
                max_depth: 1,
            },
        ] {
            assert_eq!(
                store.query_typed(q, Cancellation::default()).await,
                Err(GraphError::InvalidInput)
            );
        }
    }
    assert_eq!(
        store
            .query_typed(
                GraphQuery::Neighborhood {
                    node_id: "p".into(),
                    max_depth: 6
                },
                Cancellation::default()
            )
            .await,
        Err(GraphError::InvalidInput)
    );
    for depth in [json!(-1), json!(true), json!(1.5), json!("1")] {
        assert!(
            serde_json::from_value::<GraphQuery>(
                json!({"query":"neighborhood","node_id":"p","max_depth":depth})
            )
            .is_err()
        );
    }
    assert!(PreparedPaper::new("p", "Title", &[], &strings(&["topic"])).is_err());
    assert!(
        PreparedPaper::new(
            "author:collision",
            "Title",
            &strings(&["a"]),
            &strings(&["t"])
        )
        .is_err()
    );
    assert_eq!(
        store.status(Cancellation::default()).await.unwrap().papers,
        1
    );
}

#[tokio::test]
async fn cancellation_reset_and_sealed_token_reuse() {
    let root = tempfile::tempdir().unwrap();
    let store = open(root.path()).await;
    let c = Cancellation::default();
    store
        .apply(
            vec![replacement("p", "Title", &["Ada"], &["Graphs"])],
            c.clone(),
        )
        .await
        .unwrap();
    assert!(!c.cancel());
    assert_eq!(store.reset(c).await, Err(GraphError::AlreadySealed));
    let c = Cancellation::default();
    c.cancel();
    assert_eq!(store.reset(c.clone()).await, Err(GraphError::Cancelled));
    assert_eq!(store.status(c).await, Err(GraphError::Cancelled));
    assert_eq!(
        store.status(Cancellation::default()).await.unwrap().papers,
        1
    );
    store.reset(Cancellation::default()).await.unwrap();
    store.close().await.unwrap();
    let reopened = open(root.path()).await;
    assert_eq!(
        reopened.status(Cancellation::default()).await.unwrap(),
        GraphStatus {
            papers: 0,
            authors: 0,
            topics: 0,
            edges: 0
        }
    );
}

#[tokio::test]
async fn sqlite_cozo_compatibility_lifecycle_lock_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let sql = sqlite::open(root.path().join("history.sqlite3")).unwrap();
    sql.execute("CREATE TABLE sample (id INTEGER); INSERT INTO sample VALUES (7)")
        .unwrap();
    let store = open(root.path()).await;
    assert!(matches!(
        CozoGraphStore::open(
            root.path().into(),
            root.path().join("graph.sqlite3"),
            Cancellation::default()
        )
        .await,
        Err(GraphError::Locked)
    ));
    store
        .apply(
            vec![replacement("p", "Title", &["Ada"], &["Graphs"])],
            Cancellation::default(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    store.close().await.unwrap();
    assert_eq!(
        store.status(Cancellation::default()).await,
        Err(GraphError::Closed)
    );
    assert_eq!(
        open(root.path())
            .await
            .status(Cancellation::default())
            .await
            .unwrap()
            .papers,
        1
    );
    let mut row = sql.prepare("SELECT id FROM sample").unwrap();
    assert_eq!(row.next().unwrap(), sqlite::State::Row);
    assert_eq!(row.read::<i64, _>(0).unwrap(), 7);
}

#[tokio::test]
async fn open_refuses_unmarked_schema_invalid_paths_and_cancelled_creation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("graph.sqlite3");
    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert!(matches!(
        CozoGraphStore::open(root.path().into(), path.clone(), cancelled).await,
        Err(GraphError::Cancelled)
    ));
    assert!(!path.exists());
    let db = cozo::DbInstance::new("sqlite", &path, "").unwrap();
    db.run_default(":create paper {wrong_id: String}").unwrap();
    drop(db);
    assert!(matches!(
        CozoGraphStore::open(root.path().into(), path.clone(), Cancellation::default()).await,
        Err(GraphError::InvalidSchema)
    ));
    assert!(matches!(
        CozoGraphStore::open(
            root.path().into(),
            root.path().join("child/graph.sqlite3"),
            Cancellation::default()
        )
        .await,
        Err(GraphError::InvalidPath)
    ));
    #[cfg(unix)]
    {
        let alias = root.path().join("alias.sqlite3");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(matches!(
            CozoGraphStore::open(root.path().into(), alias, Cancellation::default()).await,
            Err(GraphError::InvalidPath)
        ));
    }
}

#[tokio::test]
async fn graph_process_lock_child() {
    let Some(root) = std::env::var_os("GRAPH_TEST_TEMP_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let result = CozoGraphStore::open(
        root.clone(),
        root.join("graph.sqlite3"),
        Cancellation::default(),
    )
    .await;
    if std::env::var_os("GRAPH_TEST_EXPECT_LOCKED").is_some() {
        assert!(matches!(result, Err(GraphError::Locked)));
    } else {
        result.unwrap().close().await.unwrap();
    }
}

#[tokio::test]
async fn lock_is_exclusive_across_processes_and_released_on_close() {
    let root = tempfile::tempdir().unwrap();
    let store = open(root.path()).await;
    let child = |locked| {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "graph_process_lock_child", "--nocapture"])
            .env("GRAPH_TEST_TEMP_ROOT", root.path());
        if locked {
            command.env("GRAPH_TEST_EXPECT_LOCKED", "1");
        } else {
            command.env_remove("GRAPH_TEST_EXPECT_LOCKED");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    child(true);
    store.close().await.unwrap();
    child(false);
}
