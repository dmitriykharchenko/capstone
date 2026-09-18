use cozo::{DbInstance, ScriptMutability};
use sqlite::{Connection, OpenFlags, State};
use std::collections::BTreeMap;

#[test]
fn sqlite_and_cozo_persist_and_reopen_in_the_same_process() {
    let root = tempfile::tempdir().unwrap();
    let history = root.path().join("history.sqlite3");
    let graph = root.path().join("graph.sqlite3");
    {
        let sql = sqlite::open(&history).unwrap();
        sql.execute("CREATE TABLE smoke (id INTEGER PRIMARY KEY, value TEXT); INSERT INTO smoke VALUES (7, 'history');").unwrap();
        let cozo = DbInstance::new("sqlite", &graph, "").unwrap();
        cozo.run_script(
            ":create smoke {id: Int => value: String}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
        cozo.run_script(
            "?[id, value] <- [[7, 'graph']] :put smoke {id => value}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
    }
    let sql = sqlite::open(&history).unwrap();
    let mut statement = sql.prepare("SELECT value FROM smoke WHERE id = 7").unwrap();
    assert_eq!(statement.next().unwrap(), State::Row);
    assert_eq!(statement.read::<String, _>(0).unwrap(), "history");
    let cozo = DbInstance::new("sqlite", &graph, "").unwrap();
    let rows = cozo
        .run_script(
            "?[id, value] := *smoke{id, value}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap()
        .into_json();
    assert_eq!(rows["rows"], serde_json::json!([[7, "graph"]]));
}

#[test]
fn readonly_vacuum_snapshot_includes_committed_wal_without_changing_source() {
    // Compatibility proof only. The history task owns production migration,
    // destination validation, schema/ID verification, publication and recovery.
    let root = tempfile::tempdir().unwrap();
    let source_path = root.path().join("source.sqlite3");
    let snapshot_path = root.path().join("snapshot.sqlite3");
    let writer = sqlite::open(&source_path).unwrap();
    writer.execute("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE smoke (id INTEGER PRIMARY KEY); INSERT INTO smoke VALUES (7);").unwrap();
    let before = std::fs::read(&source_path).unwrap();
    let wal_path = root.path().join("source.sqlite3-wal");
    let wal_before = std::fs::read(&wal_path).unwrap();
    assert!(!wal_before.is_empty());
    writer
        .execute("BEGIN; INSERT INTO smoke VALUES (8);")
        .unwrap();
    let source =
        Connection::open_with_flags(&source_path, OpenFlags::new().with_read_only()).unwrap();
    let mut snapshot = source.prepare("VACUUM INTO ?").unwrap();
    snapshot.bind((1, snapshot_path.to_str().unwrap())).unwrap();
    assert_eq!(snapshot.next().unwrap(), State::Done);
    let copy = sqlite::open(&snapshot_path).unwrap();
    let mut rows = copy.prepare("SELECT id FROM smoke").unwrap();
    assert_eq!(rows.next().unwrap(), State::Row);
    assert_eq!(rows.read::<i64, _>(0).unwrap(), 7);
    assert_eq!(rows.next().unwrap(), State::Done);
    assert_eq!(std::fs::read(&source_path).unwrap(), before);
    assert_eq!(std::fs::read(&wal_path).unwrap(), wal_before);
    writer.execute("ROLLBACK").unwrap();
}
