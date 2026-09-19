use super::*;
use std::fs;
#[path = "../../tests/papers_fixtures/mod.rs"]
mod fixtures;
use fixtures::{Page, pdf};

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn source_snapshot_runs_inside_request_owned_preparation() {
    crate::retrieval::recovery_fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let bytes = pdf(vec![Page::Text(vec![b"Linked source".to_vec()])]);
        fs::write(root.path().join("p.pdf"), &bytes).unwrap();
        let repo = NativePaperRepository::new(root.path()).unwrap();
        let owner = std::thread::current().id();
        let snapshot = group
            .run_preparation("capture source", move |c| async move {
                assert_ne!(std::thread::current().id(), owner);
                repo.prepare_for_index("p", &c).map_err(Into::into)
            })
            .await
            .unwrap();
        assert_eq!(snapshot.source_sha256, hash(&bytes));
        assert_eq!(snapshot.chunks[0].text, "Linked source");
        assert_eq!(fs::read(root.path().join("p.pdf")).unwrap(), bytes);
    })
    .await;
}

#[test]
fn source_snapshot_keeps_hash_metadata_and_chunks_on_replacement_or_later_mutation() {
    for replace in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("DBLP_conf_Test.pdf");
        let original = pdf(vec![Page::Text(vec![b"Original".to_vec()])]);
        let changed = pdf(vec![Page::Text(vec![b"Changed!".to_vec()])]);
        fs::write(&path, &original).unwrap();
        let repo = NativePaperRepository::new(root.path()).unwrap();
        let c = Cancellation::default();
        let identity = repo.source_identity("DBLP_conf_Test", &c).unwrap();
        let snapshot = repo
            .prepare_inner("DBLP_conf_Test", &c, || {
                if replace {
                    let next = root.path().join("replacement.pdf");
                    fs::write(&next, &changed).unwrap();
                    fs::rename(next, &path).unwrap();
                } else {
                    fs::write(&path, &changed).unwrap();
                }
            })
            .unwrap();
        assert_eq!(snapshot.identity, identity);
        assert_eq!(snapshot.source_sha256, hash(&original));
        assert_eq!(snapshot.info.size_bytes, original.len() as u64);
        assert_eq!(snapshot.info.title, "Test");
        assert_eq!(snapshot.chunks.len(), 1);
        assert_eq!(snapshot.chunks[0].text, "Original");
        assert_ne!(
            repo.source_identity("DBLP_conf_Test", &c).unwrap(),
            identity
        );
        let next = repo.prepare_for_index("DBLP_conf_Test", &c).unwrap();
        assert_eq!(next.source_sha256, hash(&changed));
        assert_eq!(next.chunks[0].text, "Changed!");
        assert_ne!(next.source_sha256, snapshot.source_sha256);
        assert_eq!(next.chunks.len(), snapshot.chunks.len());
        assert_eq!(fs::read(path).unwrap(), changed);
    }
}

#[test]
fn source_snapshot_rejects_mutation_within_capture_and_checks_cancellation_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("p.pdf");
    let original = pdf(vec![Page::Text(vec![b"Original".to_vec()])]);
    let repo = NativePaperRepository::new(root.path()).unwrap();
    for data in [
        b"short".to_vec(),
        vec![b'x'; original.len() + 1],
        vec![b'x'; original.len()],
    ] {
        fs::write(&path, &original).unwrap();
        assert_eq!(
            repo.capture("p", &Cancellation::default(), || fs::write(&path, data)
                .unwrap())
                .unwrap_err(),
            PaperError::ChangedDuringRead
        );
    }
    fs::write(&path, &original).unwrap();
    let c = Cancellation::default();
    c.cancel();
    assert_eq!(
        repo.prepare_for_index("p", &c).unwrap_err(),
        PaperError::Cancelled
    );
    let c = Cancellation::default();
    assert_eq!(
        repo.prepare_inner("p", &c, || {
            c.cancel();
        })
        .unwrap_err(),
        PaperError::Cancelled
    );
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn source_snapshot_shared_chunk_utf8_pages_empty_corrupt_limits_and_containment() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("p.pdf");
    let repo = NativePaperRepository::new(root.path()).unwrap();
    let c = Cancellation::default();
    let bytes = pdf(vec![
        Page::Text(vec![b"First".to_vec()]),
        Page::Image,
        Page::Blank,
        Page::Text(vec![vec![0xe9; 9001]]),
    ]);
    fs::write(&path, &bytes).unwrap();
    let snapshot = repo.prepare_for_index("p", &c).unwrap();
    let ordinary = repo.chunks("p", &c).unwrap();
    assert_eq!(
        serde_json::to_value(&snapshot.chunks).unwrap(),
        serde_json::to_value(ordinary).unwrap()
    );
    assert_eq!(snapshot.source_sha256, hash(&bytes));
    assert_eq!(snapshot.chunks[0].source.page, 1);
    for (index, chunk) in snapshot.chunks.iter().enumerate() {
        assert_eq!(chunk.source.chunk_index, index as u32);
        assert!(chunk.text.len() <= 8000);
        if index > 0 {
            assert_eq!(chunk.source.page, 4);
            assert!(chunk.text.chars().all(|c| c == 'é'));
        }
    }
    for pages in [vec![], vec![Page::Blank], vec![Page::Image]] {
        let bytes = pdf(pages);
        fs::write(&path, &bytes).unwrap();
        let empty = repo.prepare_for_index("p", &c).unwrap();
        assert!(empty.chunks.is_empty());
        assert_eq!(empty.source_sha256, hash(&bytes));
    }
    for (bytes, error) in [
        (vec![], PaperError::EmptyFile),
        (b"not PDF".to_vec(), PaperError::CorruptPdf),
        (pdf(vec![Page::Corrupt]), PaperError::CorruptPdf),
    ] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(repo.prepare_for_index("p", &c).unwrap_err(), error);
    }
    let file = fs::File::create(&path).unwrap();
    file.set_len(MAX_PDF_BYTES + 1).unwrap();
    assert_eq!(
        repo.prepare_for_index("p", &c).unwrap_err(),
        PaperError::TooLarge
    );
    assert_eq!(
        repo.prepare_for_index("../p", &c).unwrap_err(),
        PaperError::InvalidId
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&path, root.path().join("link.pdf")).unwrap();
        assert_eq!(
            repo.prepare_for_index("link", &c).unwrap_err(),
            PaperError::UnsafePath
        );
    }
}
