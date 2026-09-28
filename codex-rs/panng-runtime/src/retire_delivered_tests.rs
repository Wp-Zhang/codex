use crate::*;
use pretty_assertions::assert_eq;

fn id(number: u64) -> String {
    format!("00000000-0000-4000-8000-{number:012x}")
}
fn branch() -> BranchId {
    BranchId::new(id(1)).unwrap()
}
fn epoch(number: u64) -> ExecutionEpoch {
    ExecutionEpoch::new(id(number)).unwrap()
}
fn operation() -> OperationRecord {
    OperationRecord::prepared(
        OperationIdentity {
            id: OperationId::new(id(10)).unwrap(),
            origin_branch: branch(),
            owner_epoch: epoch(2),
            turn_id: TurnId::new("turn".into()).unwrap(),
            logical_step_id: LogicalStepId::new(id(11)).unwrap(),
            call_id: CallId::new("call".into()).unwrap(),
            provider_response_id: None,
        },
        DispatchDescriptor::new(
            ToolName::new("exec_command".into()).unwrap(),
            BoundedJson::new("{}".into()).unwrap(),
            Digest::new("a".repeat(64)).unwrap(),
            Digest::new("b".repeat(64)).unwrap(),
            Digest::new("c".repeat(64)).unwrap(),
        ),
    )
}
fn ready() -> OperationRecord {
    operation()
        .ready(
            ResultId::new(id(12)).unwrap(),
            ExecutionOutcome::Succeeded,
            StoredResult::inline(ResultEncoding::CodexResponseEnvelopeV1, "{}".into()).unwrap(),
        )
        .unwrap()
}

#[test]
fn retirement_fences_delivered_window_and_allows_new_epoch_intents() {
    let delivered = ready().in_context(CommitId::new(id(13)).unwrap()).unwrap();
    let old = RuntimeSnapshot::new(epoch(2), vec![(2, delivered)]).unwrap();
    let empty = RuntimeSnapshot::new(epoch(3), Vec::new()).unwrap();
    let mutation =
        RuntimeMutation::snapshot(empty.clone(), SnapshotReason::RetireDelivered).unwrap();
    assert_eq!(
        mutation.apply(&branch(), Some((&branch(), &old))).unwrap(),
        empty
    );
    assert_eq!(
        serde_json::from_slice::<RuntimeMutation>(&serde_json::to_vec(&mutation).unwrap()).unwrap(),
        mutation
    );
    let stale =
        RuntimeMutation::delta(vec![OperationChange::new(None, 0, operation()).unwrap()]).unwrap();
    assert!(stale.apply(&branch(), Some((&branch(), &empty))).is_err());
    let mut identity = operation().identity().clone();
    identity.owner_epoch = epoch(3);
    let fresh = OperationRecord::prepared(identity, operation().descriptor().clone());
    let delta = RuntimeMutation::delta(vec![OperationChange::new(None, 0, fresh.clone()).unwrap()])
        .unwrap();
    assert_eq!(
        delta.apply(&branch(), Some((&branch(), &empty))).unwrap(),
        RuntimeSnapshot::new(epoch(3), vec![(0, fresh)]).unwrap()
    );
}

#[test]
fn retirement_rejects_unsettled_results_and_preserves_old_snapshot_contracts() {
    let empty = RuntimeSnapshot::new(epoch(3), Vec::new()).unwrap();
    let retire = RuntimeMutation::snapshot(empty.clone(), SnapshotReason::RetireDelivered).unwrap();
    for (revision, record) in [(0, operation()), (1, ready()), (1, operation().inherited())] {
        let old = RuntimeSnapshot::new(epoch(2), vec![(revision, record)]).unwrap();
        assert!(retire.apply(&branch(), Some((&branch(), &old))).is_err());
    }
    let delivered = ready().in_context(CommitId::new(id(13)).unwrap()).unwrap();
    let old = RuntimeSnapshot::new(epoch(2), vec![(2, delivered.clone())]).unwrap();
    assert!(
        retire
            .apply(&BranchId::new(id(9)).unwrap(), Some((&branch(), &old)))
            .is_err()
    );
    let same_epoch = RuntimeMutation::snapshot(
        RuntimeSnapshot::new(epoch(2), Vec::new()).unwrap(),
        SnapshotReason::RetireDelivered,
    )
    .unwrap();
    assert!(
        same_epoch
            .apply(&branch(), Some((&branch(), &old)))
            .is_err()
    );
    assert!(
        RuntimeMutation::snapshot(
            RuntimeSnapshot::new(epoch(3), vec![(2, delivered)]).unwrap(),
            SnapshotReason::RetireDelivered
        )
        .is_err()
    );
    for reason in [SnapshotReason::Compaction, SnapshotReason::ReplayBound] {
        assert!(
            RuntimeMutation::snapshot(empty.clone(), reason)
                .unwrap()
                .apply(&branch(), Some((&branch(), &old)))
                .is_err()
        );
    }
    assert!(retire.apply(&branch(), None).is_err());
}

#[test]
fn fork_unknown_delivery_never_reexecutes_and_can_retire() {
    let parent = RuntimeSnapshot::new(epoch(2), vec![(0, operation())]).unwrap();
    let child_branch = BranchId::new(id(20)).unwrap();
    let child = parent.fork(epoch(3)).unwrap();
    let fork = RuntimeMutation::snapshot(child.clone(), SnapshotReason::Fork).unwrap();
    assert_eq!(
        fork.apply(&child_branch, Some((&branch(), &parent)))
            .unwrap(),
        child
    );
    let delivered = child.operations()[0]
        .1
        .inherited_in_context(
            ResultId::new(id(21)).unwrap(),
            CommitId::new(id(22)).unwrap(),
        )
        .unwrap();
    let delta = RuntimeMutation::delta(vec![
        OperationChange::new(Some(1), 2, delivered.clone()).unwrap(),
    ])
    .unwrap();
    let settled = RuntimeSnapshot::new(epoch(3), vec![(2, delivered)]).unwrap();
    assert_eq!(
        delta
            .apply(&child_branch, Some((&child_branch, &child)))
            .unwrap(),
        settled
    );
    assert!(
        delta
            .apply(&child_branch, Some((&child_branch, &settled)))
            .is_err()
    );
    assert!(
        operation()
            .inherited_in_context(
                ResultId::new(id(23)).unwrap(),
                CommitId::new(id(24)).unwrap()
            )
            .is_err()
    );
    let retired = RuntimeSnapshot::new(epoch(4), Vec::new()).unwrap();
    assert_eq!(
        RuntimeMutation::snapshot(retired.clone(), SnapshotReason::RetireDelivered)
            .unwrap()
            .apply(&child_branch, Some((&child_branch, &settled)))
            .unwrap(),
        retired
    );
}
