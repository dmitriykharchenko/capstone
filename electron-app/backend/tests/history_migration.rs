use research_backend::{
    history::{HistoryError, SqliteHistory},
    runtime::Cancellation,
};
use std::{fs, path::Path};

const LEGACY:&str="
CREATE TABLE exchanges(id INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL);
CREATE TABLE messages(id INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL,role TEXT NOT NULL,content TEXT NOT NULL,exchange_id INTEGER REFERENCES exchanges(id) ON DELETE CASCADE);
CREATE TABLE exchange_sources(exchange_id INTEGER NOT NULL REFERENCES exchanges(id) ON DELETE CASCADE,rank INTEGER NOT NULL,paper_id TEXT NOT NULL,chunk_index INTEGER NOT NULL,filename TEXT NOT NULL,title TEXT NOT NULL,page INTEGER NOT NULL,score REAL NOT NULL,snippet TEXT NOT NULL,PRIMARY KEY(exchange_id,rank));
CREATE TABLE paper_index_status(paper_id TEXT PRIMARY KEY,filename TEXT NOT NULL,status TEXT NOT NULL,chunk_count INTEGER NOT NULL DEFAULT 0,error TEXT,updated_at TEXT NOT NULL);
INSERT INTO exchanges VALUES(7,'session');
INSERT INTO messages VALUES(11,'session','user','question',7),(12,'session','assistant','answer',7);
INSERT INTO exchange_sources VALUES(7,1,'paper',0,'paper.pdf','Title',2,0.75,'Evidence');
INSERT INTO paper_index_status VALUES('paper','paper.pdf','indexed',3,NULL,'2026-09-18T00:00:00+00:00');
UPDATE sqlite_sequence SET seq=99 WHERE name='exchanges';
";
fn target(path: &Path) -> SqliteHistory {
    let root = path.join("new-app");
    SqliteHistory::new(&root, &root.join("history.sqlite3")).unwrap()
}
fn legacy(path: &Path) -> sqlite::Connection {
    let db = sqlite::open(path).unwrap();
    db.execute(LEGACY).unwrap();
    db
}
fn number(path: &Path, sql: &str) -> i64 {
    let db = sqlite::open(path).unwrap();
    let mut stmt = db.prepare(sql).unwrap();
    stmt.next().unwrap();
    stmt.read::<i64, _>(0).unwrap()
}

#[test]
fn import_preserves_ids_sources_statuses_sequences_and_is_idempotent_after_new_writes() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("legacy.sqlite3");
    drop(legacy(&source));
    let before = fs::read(&source).unwrap();
    let s = target(temp.path());
    let c = Cancellation::default();
    let report = s.import_legacy(&source, &c).unwrap();
    assert!(report.imported);
    assert_eq!(
        (
            report.messages,
            report.exchanges,
            report.sources,
            report.paper_statuses
        ),
        (2, 1, 1, 1)
    );
    assert_eq!(report.schema_version, 2);
    let records = s.get_messages("session").unwrap();
    assert_eq!(records[0].exchange_id, Some(7));
    assert_eq!(records[1].sources[0].snippet, "Evidence");
    assert_eq!(records[1].sources[0].page, 2);
    assert_eq!(
        s.get_paper_statuses().unwrap()[0].updated_at,
        "2026-09-18T00:00:00+00:00"
    );
    assert_eq!(
        number(s.database_path(), "SELECT MIN(id) FROM messages"),
        11
    );
    assert_eq!(
        s.append_exchange_sync("new", "q", "a", &[], &Cancellation::default())
            .unwrap(),
        100
    );
    assert!(!s.import_legacy(&source, &c).unwrap().imported);
    assert_eq!(s.get_messages("new").unwrap().len(), 2);
    assert_eq!(fs::read(source).unwrap(), before);
    assert_eq!(
        fs::read_dir(temp.path().join("new-app")).unwrap().count(),
        1
    );
}

#[test]
fn consistent_import_includes_committed_wal_excludes_uncommitted_and_preserves_source_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("wal.sqlite3");
    let db = sqlite::open(&source).unwrap();
    db.execute("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0")
        .unwrap();
    db.execute(LEGACY).unwrap();
    db.execute("INSERT INTO messages VALUES(42,'wal','system','committed',NULL)")
        .unwrap();
    let wal = temp.path().join("wal.sqlite3-wal");
    let before = fs::read(&source).unwrap();
    let wal_before = fs::read(&wal).unwrap();
    assert!(!wal_before.is_empty());
    db.execute("BEGIN; INSERT INTO messages VALUES(43,'wal','system','uncommitted',NULL)")
        .unwrap();
    let s = target(temp.path());
    assert_eq!(
        s.import_legacy(&source, &Cancellation::default())
            .unwrap()
            .messages,
        3
    );
    assert_eq!(s.get_messages("wal").unwrap().len(), 1);
    assert_eq!(s.get_messages("wal").unwrap()[0].content, "committed");
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_eq!(fs::read(&wal).unwrap(), wal_before);
    db.execute("ROLLBACK").unwrap();
    db.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    assert!(
        !s.import_legacy(&source, &Cancellation::default())
            .unwrap()
            .imported
    );
}

#[test]
fn message_only_legacy_schema_migrates_without_fabricating_exchanges() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("old.sqlite3");
    let db = sqlite::open(&source).unwrap();
    db.execute("CREATE TABLE messages(id INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL,role TEXT NOT NULL,content TEXT NOT NULL); INSERT INTO messages VALUES(42,'old','human','original')").unwrap();
    drop(db);
    let before = fs::read(&source).unwrap();
    let s = target(temp.path());
    let c = Cancellation::default();
    assert_eq!(s.import_legacy(&source, &c).unwrap().exchanges, 0);
    assert_eq!(s.get_messages("old").unwrap()[0].exchange_id, None);
    assert_eq!(number(s.database_path(), "SELECT id FROM messages"), 42);
    s.append_exchange_sync("old", "new", "reply", &[], &Cancellation::default())
        .unwrap();
    assert_eq!(
        number(s.database_path(), "SELECT MAX(id) FROM messages"),
        44
    );
    assert!(!s.import_legacy(&source, &c).unwrap().imported);
    assert_eq!(fs::read(source).unwrap(), before);
}

#[test]
fn invalid_schemas_and_data_never_publish_and_cleanup_only_private_staging() {
    for corruption in [
        "PRAGMA user_version=999",
        "CREATE TRIGGER unsafe AFTER INSERT ON messages BEGIN DELETE FROM messages; END",
        "UPDATE messages SET session_id='wrong' WHERE id=11",
        "DELETE FROM messages WHERE id=12",
        "UPDATE exchange_sources SET rank=2",
        "UPDATE exchange_sources SET page=0",
        "UPDATE exchange_sources SET score='not-a-number'",
        "UPDATE paper_index_status SET chunk_count=-1",
        "UPDATE messages SET role='invalid'",
        "UPDATE exchanges SET id=-7",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("bad.sqlite3");
        let db = legacy(&source);
        db.execute(corruption).unwrap();
        drop(db);
        let before = fs::read(&source).unwrap();
        let s = target(temp.path());
        assert!(
            s.import_legacy(&source, &Cancellation::default()).is_err(),
            "{corruption}"
        );
        assert!(!s.database_path().exists());
        assert_eq!(fs::read(source).unwrap(), before);
        assert_eq!(
            fs::read_dir(temp.path().join("new-app")).unwrap().count(),
            0
        );
    }
}

#[test]
fn missing_sources_existing_destinations_and_different_imports_are_never_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let s = target(temp.path());
    let c = Cancellation::default();
    assert_eq!(
        s.import_legacy(&temp.path().join("absent.sqlite3"), &c),
        Err(HistoryError::InvalidPath)
    );
    assert!(!temp.path().join("new-app").exists());
    let source = temp.path().join("legacy.sqlite3");
    drop(legacy(&source));
    s.initialize(&c).unwrap();
    let before = fs::read(s.database_path()).unwrap();
    assert_eq!(
        s.import_legacy(&source, &c),
        Err(HistoryError::AlreadyExists)
    );
    assert_eq!(fs::read(s.database_path()).unwrap(), before);
    let other = tempfile::tempdir().unwrap();
    let s = target(other.path());
    s.import_legacy(&source, &Cancellation::default()).unwrap();
    let before = fs::read(s.database_path()).unwrap();
    let db = sqlite::open(&source).unwrap();
    db.execute("UPDATE messages SET content='changed' WHERE id=11")
        .unwrap();
    drop(db);
    assert_eq!(
        s.import_legacy(&source, &c),
        Err(HistoryError::AlreadyExists)
    );
    assert_eq!(fs::read(s.database_path()).unwrap(), before);
}

#[test]
#[cfg(unix)]
fn symlink_and_hardlink_aliases_are_rejected_without_touching_source() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("legacy.sqlite3");
    drop(legacy(&source));
    let before = fs::read(&source).unwrap();
    let link = temp.path().join("link.sqlite3");
    symlink(&source, &link).unwrap();
    let s = target(temp.path());
    let c = Cancellation::default();
    assert_eq!(s.import_legacy(&link, &c), Err(HistoryError::InvalidPath));
    fs::create_dir(temp.path().join("new-app")).unwrap();
    fs::hard_link(&source, s.database_path()).unwrap();
    assert_eq!(s.import_legacy(&source, &c), Err(HistoryError::InvalidPath));
    assert_eq!(fs::read(source).unwrap(), before);
}

#[test]
fn concurrent_initialization_never_clobbers_published_database() {
    let temp = tempfile::tempdir().unwrap();
    let s = target(temp.path());
    let other = target(temp.path());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let b = barrier.clone();
    let thread = std::thread::spawn(move || {
        b.wait();
        other.initialize(&Cancellation::default())
    });
    barrier.wait();
    let a = s.initialize(&Cancellation::default());
    let b = thread.join().unwrap();
    assert!(a.is_ok() || b.is_ok());
    assert!(a.is_ok() || a == Err(HistoryError::AlreadyExists));
    assert!(b.is_ok() || b == Err(HistoryError::AlreadyExists));
    s.initialize(&Cancellation::default()).unwrap();
}

#[test]
fn already_cancelled_import_and_initialize_make_no_destination_changes() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("legacy.sqlite3");
    drop(legacy(&source));
    let before = fs::read(&source).unwrap();
    let s = target(temp.path());
    let cancellation = Cancellation::default();
    assert!(cancellation.cancel());
    assert_eq!(s.initialize(&cancellation), Err(HistoryError::Cancelled));
    assert_eq!(
        s.import_legacy(&source, &cancellation),
        Err(HistoryError::Cancelled)
    );
    assert!(!temp.path().join("new-app").exists());
    assert_eq!(fs::read(source).unwrap(), before);
}
