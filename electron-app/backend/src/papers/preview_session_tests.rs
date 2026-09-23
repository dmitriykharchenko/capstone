use super::*;
use std::{fs, io::Write};

fn fixture() -> (tempfile::TempDir, Arc<SafeRoot>, Arc<Sessions>) {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("p.pdf"), b"%PDF-1.4\nfixture").unwrap();
    let root = Arc::new(SafeRoot::new(temp.path()).unwrap());
    (temp, root, Arc::default())
}

#[test]
fn large_sparse_source_reads_only_requested_ranges_without_capture_or_index_limit() {
    let (temp, root, sessions) = fixture();
    let file = fs::OpenOptions::new()
        .write(true)
        .open(temp.path().join("p.pdf"))
        .unwrap();
    let length = 65 * 1024 * 1024 + 7;
    file.set_len(length).unwrap();
    let lease = sessions
        .open(root, "p".into(), Cancellation::default())
        .unwrap();
    assert_eq!(lease.info().total_bytes, length);
    assert_eq!(
        sessions
            .read(&lease.id, 0, 5, &Cancellation::default())
            .unwrap(),
        b"%PDF-"
    );
    assert_eq!(
        sessions
            .read(
                &lease.id,
                length - MAX_PREVIEW_RANGE,
                length,
                &Cancellation::default()
            )
            .unwrap(),
        vec![0; MAX_PREVIEW_RANGE as usize]
    );
    let session = lease.session.clone();
    drop(lease);
    assert!(session.data.lock().unwrap().file.is_none());
    assert!(sessions.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_ranges_fail_closed_and_drop_the_descriptor() {
    for (begin, end) in [
        (0, 0),
        (2, 1),
        (0, 17),
        (0, MAX_PREVIEW_RANGE + 1),
        (u64::MAX, u64::MAX),
    ] {
        let (_temp, root, sessions) = fixture();
        let lease = sessions
            .open(root, "p".into(), Cancellation::default())
            .unwrap();
        assert_eq!(
            sessions.read(&lease.id, begin, end, &Cancellation::default()),
            Err(ErrorCode::PdfInvalid)
        );
        assert_eq!(lease.wait().await, ErrorCode::PdfInvalid);
        assert!(lease.session.data.lock().unwrap().file.is_none());
        assert_eq!(
            sessions.read(&lease.id, 0, 1, &Cancellation::default()),
            Err(ErrorCode::PdfInvalid)
        );
    }
}

#[tokio::test]
async fn observed_replacement_truncation_and_same_size_changes_close_without_mixed_bytes() {
    for change in ["replace", "truncate", "rewrite", "unlink", "symlink"] {
        let (temp, root, sessions) = fixture();
        let path = temp.path().join("p.pdf");
        let lease = sessions
            .open(root, "p".into(), Cancellation::default())
            .unwrap();
        assert_eq!(
            sessions
                .read(&lease.id, 0, 5, &Cancellation::default())
                .unwrap(),
            b"%PDF-"
        );
        match change {
            "replace" => {
                fs::write(temp.path().join("replacement"), b"%PDF-1.4\nchanged").unwrap();
                fs::rename(temp.path().join("replacement"), &path).unwrap();
            }
            "truncate" => fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(2)
                .unwrap(),
            "rewrite" => {
                let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.write_all(b"%PDF-2.0\nchanged").unwrap();
                file.set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
                    .unwrap();
            }
            "unlink" => fs::remove_file(&path).unwrap(),
            "symlink" => {
                fs::rename(&path, temp.path().join("old")).unwrap();
                std::os::unix::fs::symlink(temp.path().join("old"), &path).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            sessions.read(&lease.id, 0, 1, &Cancellation::default()),
            Err(ErrorCode::PdfChanged)
        );
        assert_eq!(lease.wait().await, ErrorCode::PdfChanged);
        assert!(lease.session.data.lock().unwrap().file.is_none());
    }
}

#[test]
fn session_admission_cancel_unknown_and_drop_remain_bounded() {
    let (_temp, root, sessions) = fixture();
    let mut leases = Vec::new();
    for _ in 0..MAX_PREVIEW_SESSIONS {
        leases.push(
            sessions
                .open(root.clone(), "p".into(), Cancellation::default())
                .unwrap(),
        );
    }
    assert!(matches!(
        sessions.open(root.clone(), "p".into(), Cancellation::default()),
        Err(ErrorCode::Busy)
    ));
    assert_eq!(
        sessions.read("unknown", 0, 1, &Cancellation::default()),
        Err(ErrorCode::PdfClosed)
    );
    let stale = leases[0].id.clone();
    leases.clear();
    assert!(sessions.0.lock().unwrap().is_empty());
    assert_eq!(
        sessions.read(&stale, 0, 1, &Cancellation::default()),
        Err(ErrorCode::PdfClosed)
    );
    let token = Cancellation::default();
    let lease = sessions
        .open(root.clone(), "p".into(), token.clone())
        .unwrap();
    token.cancel();
    assert_eq!(
        sessions.read(&lease.id, 0, 1, &Cancellation::default()),
        Err(ErrorCode::PdfClosed)
    );
    assert!(lease.session.data.lock().unwrap().file.is_none());
    assert!(matches!(
        sessions.open(root, "p".into(), token),
        Err(ErrorCode::Cancelled)
    ));
}

#[tokio::test]
async fn idle_expiry_and_explicit_close_release_descriptors() {
    for idle in [true, false] {
        let (_temp, root, sessions) = fixture();
        let lease = sessions
            .open(root, "p".into(), Cancellation::default())
            .unwrap();
        if idle {
            lease.session.data.lock().unwrap().touched = Instant::now() - IDLE_TIMEOUT;
        } else {
            sessions.close(&lease.id, ErrorCode::PdfClosed);
        }
        assert_eq!(lease.wait().await, ErrorCode::PdfClosed);
        assert!(lease.session.data.lock().unwrap().file.is_none());
    }
}

#[test]
fn pinned_root_never_switches_to_replacement_directory() {
    let (temp, root, sessions) = fixture();
    let lease = sessions
        .open(root, "p".into(), Cancellation::default())
        .unwrap();
    let parent = tempfile::tempdir().unwrap();
    let moved = parent.path().join("old");
    fs::rename(temp.path(), &moved).unwrap();
    fs::create_dir(temp.path()).unwrap();
    fs::write(temp.path().join("p.pdf"), b"outside").unwrap();
    assert_eq!(
        sessions
            .read(&lease.id, 0, 5, &Cancellation::default())
            .unwrap(),
        b"%PDF-"
    );
}

#[test]
fn concurrent_ranges_serialize_without_spurious_busy_or_mixed_offsets() {
    let (_temp, root, sessions) = fixture();
    let lease = sessions
        .open(root, "p".into(), Cancellation::default())
        .unwrap();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..32 {
                    assert_eq!(
                        sessions
                            .read(&lease.id, 1, 5, &Cancellation::default())
                            .unwrap(),
                        b"PDF-"
                    );
                }
            });
        }
    });
}
