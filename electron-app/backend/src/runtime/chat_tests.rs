use super::*;

#[tokio::test]
async fn authoritative_seal_races_stop_and_keeps_late_cancel_refused() {
    for stop_first in [false, true] {
        let group = OperationGroup::new();
        let stop = group.preparation_stop();
        let cancel = stop.clone();
        let outcome = group
            .run_authoritative_mutation(move |c| async move {
                if stop_first {
                    assert!(cancel.cancel());
                }
                let permit = c.begin_commit();
                if stop_first {
                    assert!(matches!(permit, Err(ErrorCode::Cancelled)));
                    return Err(ErrorCode::Cancelled);
                }
                let _permit = permit?;
                assert!(!cancel.cancel());
                Ok(())
            })
            .await;
        if stop_first {
            assert_eq!(outcome, Err(ErrorCode::Cancelled));
            assert_eq!(group.finish(outcome), Err(ErrorCode::Cancelled));
        } else {
            outcome.unwrap();
            assert!(!stop.cancel());
            assert_eq!(
                group
                    .run_derived(|_| async { Err::<(), _>(ErrorCode::Internal) })
                    .await
                    .unwrap(),
                Err(ErrorCode::Internal)
            );
            let _permit = group.seal_completion().unwrap();
            assert_eq!(group.finish(Ok(())), Ok(()));
            assert!(!stop.cancel());
        }
    }
}
#[tokio::test]
async fn uncontained_post_commit_panic_is_unknown_not_saved_success() {
    let group = OperationGroup::new();
    group
        .run_authoritative_mutation(|c| async move {
            let _permit = c.begin_commit()?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        group
            .run_derived(|_| async {
                panic!("synthetic panic");
                #[allow(unreachable_code)]
                Ok(())
            })
            .await
            .is_err()
    );
    assert_eq!(group.finish(Ok(())), Err(ErrorCode::OutcomeUnknown));
}
