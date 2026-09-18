use research_backend::{
    domain::SourceRecord,
    history::{HistoryError, HistoryStore, SqliteHistory},
    runtime::Cancellation,
};
use std::{fs, path::Path};

fn store(temp: &Path) -> SqliteHistory {
    let root = temp.join("app");
    let store = SqliteHistory::new(&root, &root.join("history.sqlite3")).unwrap();
    store.initialize(&Cancellation::default()).unwrap();
    store
}
fn source(rank: u32) -> SourceRecord {
    SourceRecord {
        rank,
        paper_id: "paper".into(),
        chunk_index: rank - 1,
        filename: "paper.pdf".into(),
        title: "Title".into(),
        page: 1,
        score: 0.9,
        snippet: "Evidence 🦀".into(),
    }
}

#[test]
fn exchanges_messages_sessions_and_statuses_survive_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let s = store(temp.path());
    let c = Cancellation::default();
    s.append_message("one", " SYSTEM ", "  exact whitespace  ", &c)
        .unwrap();
    assert!(!c.cancel());
    let id = s
        .append_exchange_sync(
            "one",
            "one two three four five six seven",
            "answer",
            &[source(1)],
            &Cancellation::default(),
        )
        .unwrap();
    s.append_exchange_sync("two", "other", "reply", &[], &Cancellation::default())
        .unwrap();
    s.record_paper_status("p", "p.pdf", "indexed", 42, None, &Cancellation::default())
        .unwrap();
    s.record_paper_status(
        "p",
        "p.pdf",
        "failed",
        0,
        Some("failure"),
        &Cancellation::default(),
    )
    .unwrap();
    drop(s);
    let s = store(temp.path());
    let messages = s.get_messages("one").unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].content, "  exact whitespace  ");
    assert_eq!(messages[1].exchange_id, Some(id));
    assert!(messages[1].sources.is_empty());
    assert_eq!(messages[2].sources, vec![source(1)]);
    assert_eq!(
        s.get_exchange(id, "one").unwrap().assistant_message.content,
        "answer"
    );
    assert!(matches!(
        s.get_exchange(id, "two"),
        Err(HistoryError::NotFound)
    ));
    let sessions = s.list_sessions().unwrap();
    assert_eq!(sessions[0].session_id, "two");
    assert_eq!(sessions[1].title, "one two three four five six…");
    assert_eq!(sessions[1].message_count, 3);
    assert_eq!(sessions[1].exchange_count, 1);
    assert!(s.get_messages("unknown").unwrap().is_empty());
    let statuses = s.get_paper_statuses().unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].status, "failed");
    assert!(chrono::DateTime::parse_from_rfc3339(&statuses[0].updated_at).is_ok());
    s.clear_paper_statuses(&Cancellation::default()).unwrap();
    assert!(s.get_paper_statuses().unwrap().is_empty());
    assert_eq!(s.get_messages("one").unwrap().len(), 3);
}

#[test]
fn source_bounds_and_inputs_validate_before_writes() {
    let temp = tempfile::tempdir().unwrap();
    let s = store(temp.path());
    let c = Cancellation::default();
    let sources = (1..=35).map(source).collect::<Vec<_>>();
    s.append_exchange_sync("s", "q", "a", &sources, &c).unwrap();
    assert_eq!(s.get_messages("s").unwrap()[1].sources.len(), 35);
    for sources in [
        vec![source(2)],
        (1..=36).map(source).collect(),
        vec![SourceRecord {
            score: f64::NAN,
            ..source(1)
        }],
        vec![SourceRecord {
            snippet: "🦀".repeat(281),
            ..source(1)
        }],
        vec![SourceRecord {
            page: 0,
            ..source(1)
        }],
    ] {
        assert_eq!(
            s.append_exchange_sync("s", "q", "a", &sources, &c),
            Err(HistoryError::InvalidInput)
        );
    }
    assert!(s.append_exchange_sync(" ", "q", "a", &[], &c).is_err());
    assert!(s.append_message("s", "nobody", "q", &c).is_err());
    assert!(s.append_message("s", "user", " ", &c).is_err());
    assert_eq!(s.get_messages("s").unwrap().len(), 2);
}

#[test]
fn failed_source_insert_rolls_back_entire_exchange() {
    let temp = tempfile::tempdir().unwrap();
    let s = store(temp.path());
    let c = Cancellation::default();
    let db = sqlite::open(s.database_path()).unwrap();
    db.execute("CREATE TRIGGER fail_source BEFORE INSERT ON exchange_sources BEGIN SELECT RAISE(ABORT,'synthetic failure'); END").unwrap();
    assert_eq!(
        s.append_exchange_sync("s", "q", "a", &[source(1)], &c),
        Err(HistoryError::Storage)
    );
    assert!(s.get_messages("s").unwrap().is_empty());
    assert!(s.list_sessions().unwrap().is_empty());
    db.execute("DROP TRIGGER fail_source").unwrap();
    assert_eq!(s.append_exchange_sync("s", "q", "a", &[], &c).unwrap(), 1);
}

#[tokio::test]
async fn async_foundation_facade_uses_real_store_without_registering_commands() {
    let temp = tempfile::tempdir().unwrap();
    let s = store(temp.path());
    let id = s
        .append_exchange(
            "s".into(),
            "q".into(),
            "a".into(),
            vec![],
            Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        s.messages("s".into()).await.unwrap()[0].exchange_id,
        Some(id)
    );
    assert_eq!(
        s.sessions().await.unwrap()["sessions"][0]["exchange_count"],
        1
    );
}

#[test]
fn unmarked_existing_database_is_not_opened_for_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("app");
    fs::create_dir(&root).unwrap();
    let path = root.join("history.sqlite3");
    let db = sqlite::open(&path).unwrap();
    db.execute("CREATE TABLE messages(id INTEGER PRIMARY KEY)")
        .unwrap();
    drop(db);
    let before = fs::read(&path).unwrap();
    let s = SqliteHistory::new(&root, &path).unwrap();
    assert_eq!(
        s.initialize(&Cancellation::default()),
        Err(HistoryError::InvalidSchema)
    );
    assert_eq!(fs::read(path).unwrap(), before);
}
