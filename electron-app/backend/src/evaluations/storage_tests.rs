use super::*;
use crate::{
    domain::{EvaluationCase, RetrievalStrategy},
    evaluations::{CaseError, CaseResult},
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::{
        Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

fn setup() -> (tempfile::TempDir, StoragePaths, ArtifactStore) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let paths = StoragePaths {
        data_root: root.join("data"),
        evaluation_results: root.join("data/evaluations"),
        history_database: root.join("data/history.sqlite"),
        vector_database: root.join("data/vector.sqlite"),
        graph_database: root.join("data/graph.sqlite"),
        bm25_index: root.join("data/bm25.json"),
        resources: root.join("resources"),
        papers: root.join("papers"),
    };
    let store = ArtifactStore::from_paths(&paths).unwrap();
    (tmp, paths, store)
}
fn artifact(question: &str) -> EvaluationArtifact {
    let input = EvaluationCase {
        question: question.into(),
        grading_notes: "notes".into(),
        sources: vec![],
    };
    EvaluationArtifact::new(
        vec![input.clone()],
        vec![CaseResult {
            case_index: 1,
            input,
            retrieval_strategy: RetrievalStrategy::Hybrid,
            retrieved_chunks: vec![],
            generated_answer: None,
            judge_scores: None,
            error: Some(CaseError::new("planner", false)),
        }],
    )
    .unwrap()
}
fn files(paths: &StoragePaths) -> Vec<String> {
    let mut files = fs::read_dir(&paths.evaluation_results)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    files.sort();
    files
}

// Source: test_evaluation_results.py deterministic UTF-8/atomic replacement tests.
#[test]
fn stable_utf8_replacement_directory_sync_and_no_staging() {
    let (_tmp, paths, store) = setup();
    for question in ["first", "café?"] {
        store
            .publish(&artifact(question), &Cancellation::default())
            .unwrap();
        let bytes = fs::read(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap();
        assert_eq!(bytes, artifact(question).serialize().unwrap());
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(files(&paths), [RESULTS_FILENAME]);
    }
}
#[test]
fn accepted_cancel_at_each_preseal_boundary_preserves_prior_and_cleans_staging() {
    for point in [Point::Staged, Point::Synced] {
        let (_tmp, paths, store) = setup();
        let prior = artifact("prior");
        store.publish(&prior, &Cancellation::default()).unwrap();
        let token = Cancellation::default();
        let lease = store.acquire().unwrap();
        let result =
            lease.publish_inner(&artifact("next").serialize().unwrap(), &token, |at, _| {
                if point == at {
                    assert!(token.cancel());
                }
                Ok(())
            });
        assert_eq!(result, Err(ErrorCode::Cancelled));
        assert_eq!(
            fs::read(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap(),
            prior.serialize().unwrap()
        );
        assert_eq!(files(&paths), [RESULTS_FILENAME]);
    }
}
#[test]
fn sealed_cancellation_is_rejected_through_success_and_postrename_failure() {
    for fail in [false, true] {
        let (_tmp, paths, store) = setup();
        store
            .publish(&artifact("prior"), &Cancellation::default())
            .unwrap();
        let token = Cancellation::default();
        let lease = store.acquire().unwrap();
        let result =
            lease.publish_inner(&artifact("next").serialize().unwrap(), &token, |at, _| {
                if matches!(at, Point::Sealed | Point::Renamed) {
                    assert!(!token.cancel());
                }
                if at == Point::Renamed && fail {
                    // A real fsync failure, after rename. Never report rollback.
                    return File::open("/dev/null").unwrap().sync_all().map_err(io);
                }
                Ok(())
            });
        assert_eq!(
            result,
            if fail {
                Err(ErrorCode::Internal)
            } else {
                Ok(())
            }
        );
        assert!(!token.cancel());
        assert_eq!(
            fs::read(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap(),
            artifact("next").serialize().unwrap()
        );
        assert_eq!(files(&paths), [RESULTS_FILENAME]);
    }
}
#[test]
fn real_prepublication_io_failure_preserves_prior_and_cleans_owned_staging() {
    let (_tmp, paths, store) = setup();
    store
        .publish(&artifact("prior"), &Cancellation::default())
        .unwrap();
    let result = store.acquire().unwrap().publish_inner(
        &artifact("next").serialize().unwrap(),
        &Cancellation::default(),
        |at, _| {
            if at == Point::Staged {
                return File::open("/dev/null").unwrap().sync_all().map_err(io);
            }
            Ok(())
        },
    );
    assert_eq!(result, Err(ErrorCode::Internal));
    assert_eq!(
        fs::read(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap(),
        artifact("prior").serialize().unwrap()
    );
    assert_eq!(files(&paths), [RESULTS_FILENAME]);
}
#[cfg(target_os = "macos")]
#[test]
fn real_rename_failure_keeps_prior_artifact() {
    let (_tmp, paths, store) = setup();
    store
        .publish(&artifact("prior"), &Cancellation::default())
        .unwrap();
    let target = File::open(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap();
    let token = Cancellation::default();
    let result = store.acquire().unwrap().publish_inner(
        &artifact("next").serialize().unwrap(),
        &token,
        |at, _| {
            if at == Point::Sealed {
                // SAFETY: mark only the temp-test artifact immutable to force rename EPERM.
                assert_eq!(
                    unsafe { libc::fchflags(target.as_raw_fd(), libc::UF_IMMUTABLE) },
                    0
                );
            }
            Ok(())
        },
    );
    // SAFETY: restore only the same temp-test descriptor so TempDir cleanup works.
    assert_eq!(unsafe { libc::fchflags(target.as_raw_fd(), 0) }, 0);
    assert_eq!(result, Err(ErrorCode::Internal));
    assert!(!token.cancel());
    assert_eq!(
        fs::read(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap(),
        artifact("prior").serialize().unwrap()
    );
    assert_eq!(files(&paths), [RESULTS_FILENAME]);
}
#[test]
fn final_seal_race_has_exactly_one_winner_and_no_orphans() {
    for _ in 0..40 {
        let (_tmp, paths, store) = setup();
        store
            .publish(&artifact("prior"), &Cancellation::default())
            .unwrap();
        let token = Cancellation::default();
        let barrier = Arc::new(Barrier::new(2));
        let accepted = Arc::new(AtomicBool::new(false));
        let cancel = {
            let t = token.clone();
            let b = barrier.clone();
            let a = accepted.clone();
            thread::spawn(move || {
                b.wait();
                a.store(t.cancel(), Ordering::SeqCst);
            })
        };
        let result = store.acquire().unwrap().publish_inner(
            &artifact("next").serialize().unwrap(),
            &token,
            |at, _| {
                if at == Point::Synced {
                    barrier.wait();
                }
                Ok(())
            },
        );
        cancel.join().unwrap();
        let cancelled = accepted.load(Ordering::SeqCst);
        assert_eq!(
            result,
            if cancelled {
                Err(ErrorCode::Cancelled)
            } else {
                Ok(())
            }
        );
        assert_eq!(
            fs::read(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap(),
            artifact(if cancelled { "prior" } else { "next" })
                .serialize()
                .unwrap()
        );
        assert_eq!(files(&paths), [RESULTS_FILENAME]);
    }
}
#[test]
fn competing_instances_fail_fast_and_release_lock_on_drop() {
    let (_tmp, paths, store) = setup();
    let other = ArtifactStore::from_paths(&paths).unwrap();
    let first = store.acquire().unwrap();
    // Model a fork's inherited open-file description before the child's exec.
    let inherited = first.directory.try_clone().unwrap();
    assert!(matches!(other.acquire(), Err(ErrorCode::Busy)));
    drop(first);
    other
        .publish(&artifact("after drop"), &Cancellation::default())
        .unwrap();
    drop(inherited);
}
#[test]
fn prior_artifact_validation_error_unlocks_inherited_lease_for_competing_store() {
    let (_tmp, paths, store) = setup();
    let other = ArtifactStore::from_paths(&paths).unwrap();
    let prior = artifact("prior");
    store.publish(&prior, &Cancellation::default()).unwrap();
    let target = paths.evaluation_results.join(RESULTS_FILENAME);
    let next = artifact("after validation error").serialize().unwrap();
    let token = Cancellation::default();
    let inherited;
    let result = {
        let lease = store.acquire().unwrap();
        inherited = lease.directory.try_clone().unwrap();
        assert!(matches!(other.acquire(), Err(ErrorCode::Busy)));
        // Corrupt the prior artifact after acquisition to exercise read_prior's
        // validation error during publication, before the commit seal.
        fs::write(&target, "{}").unwrap();
        lease.publish(&next, &token)
    };
    assert_eq!(result, Err(ErrorCode::InvalidParams));
    assert_eq!(fs::read(&target).unwrap(), b"{}");
    assert_eq!(files(&paths), [RESULTS_FILENAME]);
    assert!(token.cancel());

    // Repair only the test fixture so the competitor can validate it. The cloned
    // open-file description stays live: close alone cannot release this lock.
    fs::write(&target, prior.serialize().unwrap()).unwrap();
    let competing = other.acquire().unwrap();
    assert!(matches!(store.acquire(), Err(ErrorCode::Busy)));
    competing.publish(&next, &Cancellation::default()).unwrap();
    assert_eq!(fs::read(&target).unwrap(), next);
    assert_eq!(files(&paths), [RESULTS_FILENAME]);
    drop(competing);
    drop(inherited);
}
#[test]
fn reject_non_evaluation_files_symlinks_hardlinks_and_directory_targets() {
    for kind in 0..4 {
        let (_tmp, paths, store) = setup();
        let other = paths.data_root.join("other");
        fs::write(&other, artifact("do not touch").serialize().unwrap()).unwrap();
        let target = paths.evaluation_results.join(RESULTS_FILENAME);
        match kind {
            0 => fs::write(&target, "unrelated data").unwrap(),
            1 => symlink(&other, &target).unwrap(),
            2 => fs::hard_link(&other, &target).unwrap(),
            _ => fs::create_dir(&target).unwrap(),
        }
        assert!(
            store
                .publish(&artifact("next"), &Cancellation::default())
                .is_err()
        );
        assert_eq!(
            fs::read(&other).unwrap(),
            artifact("do not touch").serialize().unwrap()
        );
        assert_eq!(files(&paths), [RESULTS_FILENAME]);
    }
}
#[test]
fn reject_traversal_reserved_collisions_symlinked_ancestors_and_root_swaps() {
    let (tmp, paths, store) = setup();
    for destination in [
        paths.data_root.clone(),
        paths.data_root.join("../outside"),
        paths.resources.clone(),
        paths.history_database.clone(),
    ] {
        let mut bad = paths.clone();
        bad.evaluation_results = destination;
        assert!(ArtifactStore::from_paths(&bad).is_err());
    }
    let mut collision = paths.clone();
    collision.history_database = paths.evaluation_results.join(RESULTS_FILENAME);
    assert!(ArtifactStore::from_paths(&collision).is_err());
    let mut unsafe_path = paths.clone();
    let alias = paths.data_root.join("alias");
    symlink(&paths.evaluation_results, &alias).unwrap();
    unsafe_path.evaluation_results = alias;
    assert!(ArtifactStore::from_paths(&unsafe_path).is_err());
    let moved = tmp.path().join("moved");
    fs::rename(&paths.data_root, &moved).unwrap();
    fs::create_dir_all(&paths.evaluation_results).unwrap();
    assert!(
        store
            .publish(&artifact("next"), &Cancellation::default())
            .is_err()
    );
    assert!(files(&paths).is_empty());
    assert_eq!(fs::read_dir(moved.join("evaluations")).unwrap().count(), 0);
}
#[test]
fn refuse_swapped_target_during_run_and_cleanup_without_touching_foreign_staging() {
    let (_tmp, paths, store) = setup();
    store
        .publish(&artifact("prior"), &Cancellation::default())
        .unwrap();
    let lease = store.acquire().unwrap();
    fs::write(paths.evaluation_results.join(".foreign.tmp"), "not ours").unwrap();
    fs::write(
        paths.evaluation_results.join(RESULTS_FILENAME),
        artifact("external").serialize().unwrap(),
    )
    .unwrap();
    assert!(
        lease
            .publish(
                &artifact("next").serialize().unwrap(),
                &Cancellation::default()
            )
            .is_err()
    );
    assert_eq!(
        fs::read(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap(),
        artifact("external").serialize().unwrap()
    );
    assert_eq!(files(&paths), [".foreign.tmp", RESULTS_FILENAME]);
}
#[test]
fn world_writable_output_and_invalid_complete_artifact_are_refused() {
    let (_tmp, paths, store) = setup();
    let mut bad = artifact("invalid");
    bad.results[0].case_index = 0;
    assert_eq!(
        store.publish(&bad, &Cancellation::default()),
        Err(ErrorCode::InvalidParams)
    );
    assert!(files(&paths).is_empty());
    let other = ArtifactStore::from_paths(&paths).unwrap();
    let next = artifact("after invalid serialization");
    other.publish(&next, &Cancellation::default()).unwrap();
    assert_eq!(
        fs::read(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap(),
        next.serialize().unwrap()
    );
    assert_eq!(files(&paths), [RESULTS_FILENAME]);
    fs::set_permissions(&paths.evaluation_results, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        store
            .publish(&artifact("next"), &Cancellation::default())
            .is_err()
    );
    fs::set_permissions(&paths.evaluation_results, fs::Permissions::from_mode(0o700)).unwrap();
}
