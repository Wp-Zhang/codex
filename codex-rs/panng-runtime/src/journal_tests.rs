use crate::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn branch() -> BranchId {
    BranchId::new("00000000-0000-0000-0000-000000000001".into()).unwrap()
}
fn epoch() -> ExecutionEpoch {
    ExecutionEpoch::new("00000000-0000-0000-0000-000000000002".into()).unwrap()
}
fn other_epoch() -> ExecutionEpoch {
    ExecutionEpoch::new("00000000-0000-0000-0000-000000000003".into()).unwrap()
}
fn descriptor() -> DispatchDescriptor {
    let digest = Digest::new("a".repeat(64)).unwrap();
    DispatchDescriptor::new(
        ToolName::new("exec_command".into()).unwrap(),
        BoundedJson::new("{}".into()).unwrap(),
        digest.clone(),
        digest.clone(),
        digest,
    )
}
fn operation(index: u64) -> OperationRecord {
    OperationRecord::prepared(
        OperationIdentity {
            id: OperationId::new(format!("00000000-0000-0000-0000-{index:012x}")).unwrap(),
            origin_branch: branch(),
            owner_epoch: epoch(),
            turn_id: TurnId::new("turn".into()).unwrap(),
            logical_step_id: LogicalStepId::new("00000000-0000-0000-0000-000000000004".into())
                .unwrap(),
            call_id: CallId::new(format!("call-{index}")).unwrap(),
            provider_response_id: None,
        },
        descriptor(),
    )
}
fn ready(record: &OperationRecord) -> OperationRecord {
    record
        .ready(
            ResultId::new(record.identity().id.as_str().into()).unwrap(),
            ExecutionOutcome::Succeeded,
            StoredResult::inline(
                ResultEncoding::CodexResponseEnvelopeV1,
                "{\"output\":\"done\"}".into(),
            )
            .unwrap(),
        )
        .unwrap()
}
fn commit() -> CommitId {
    CommitId::new("00000000-0000-0000-0000-000000000005".into()).unwrap()
}

#[test]
fn journal_delta_roundtrip_and_ordered_delivery_preserve_exact_results() {
    let initial = RuntimeSnapshot::new(epoch(), Vec::new()).unwrap();
    let prepared = operation(10);
    let mutation = RuntimeMutation::delta(vec![
        OperationChange::new(None, 0, prepared.clone()).unwrap(),
    ])
    .unwrap();
    let encoded = serde_json::to_vec(&mutation).unwrap();
    let decoded: RuntimeMutation = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, mutation);
    let state = decoded
        .apply(&branch(), Some((&branch(), &initial)))
        .unwrap();
    let completed = ready(&prepared);
    let ready_state = RuntimeMutation::delta(vec![
        OperationChange::new(Some(0), 1, completed.clone()).unwrap(),
    ])
    .unwrap()
    .apply(&branch(), Some((&branch(), &state)))
    .unwrap();
    let delivered = completed.in_context(commit()).unwrap();
    let final_state = RuntimeMutation::delta(vec![
        OperationChange::new(Some(1), 2, delivered.clone()).unwrap(),
    ])
    .unwrap()
    .apply(&branch(), Some((&branch(), &ready_state)))
    .unwrap();
    assert_eq!(
        final_state,
        RuntimeSnapshot::new(epoch(), vec![(2, delivered)]).unwrap()
    );
}

#[test]
fn journal_rejects_conflicting_descriptor_revision_and_outcome_rewrites_atomically() {
    let prepared = operation(10);
    let original = RuntimeSnapshot::new(epoch(), vec![(0, prepared.clone())]).unwrap();
    let completed = ready(&prepared);
    let mut wire = serde_json::to_value(&completed).unwrap();
    wire["descriptor"]["arguments"] = json!("{\"different\":true}");
    let changed: OperationRecord = serde_json::from_value(wire).unwrap();
    for change in [
        OperationChange::new(Some(0), 1, changed).unwrap(),
        OperationChange::new(Some(1), 2, completed.clone()).unwrap(),
    ] {
        assert!(
            RuntimeMutation::delta(vec![change])
                .unwrap()
                .apply(&branch(), Some((&branch(), &original)))
                .is_err()
        );
    }
    assert_eq!(
        original,
        RuntimeSnapshot::new(epoch(), vec![(0, prepared)]).unwrap()
    );
    let ready_state = RuntimeSnapshot::new(epoch(), vec![(1, completed.clone())]).unwrap();
    assert!(
        RuntimeMutation::delta(vec![OperationChange::new(Some(1), 2, completed).unwrap()])
            .unwrap()
            .apply(&branch(), Some((&branch(), &ready_state)))
            .is_err()
    );
}

#[test]
fn journal_fork_strips_pending_ownership_but_can_deliver_inherited_ready_results() {
    let completed = ready(&operation(10));
    let pending = operation(11);
    let original =
        RuntimeSnapshot::new(epoch(), vec![(1, completed.clone()), (0, pending)]).unwrap();
    let child_branch = BranchId::new("00000000-0000-0000-0000-000000000099".into()).unwrap();
    let forked = original.fork(other_epoch()).unwrap();
    let child = RuntimeMutation::snapshot(forked.clone(), SnapshotReason::Fork)
        .unwrap()
        .apply(&child_branch, Some((&branch(), &original)))
        .unwrap();
    assert_eq!(child, forked);
    assert_eq!(
        child.operations()[1].1.state(),
        &OperationState::InheritedUnknown
    );
    let delivered = completed.in_context(commit()).unwrap();
    let next = RuntimeMutation::delta(vec![
        OperationChange::new(Some(1), 2, delivered.clone()).unwrap(),
    ])
    .unwrap()
    .apply(&child_branch, Some((&child_branch, &child)))
    .unwrap();
    assert_eq!(next.operations()[0], (2, delivered));
    assert_eq!(next.operations()[1], child.operations()[1]);
    assert!(
        RuntimeMutation::delta(vec![
            OperationChange::new(Some(1), 2, ready(&operation(11))).unwrap()
        ])
        .unwrap()
        .apply(&child_branch, Some((&child_branch, &child)))
        .is_err()
    );
}

#[test]
fn journal_resume_requires_typed_unknown_and_preserves_completed_results() {
    let pending = operation(10);
    let completed = ready(&operation(11));
    let old =
        RuntimeSnapshot::new(epoch(), vec![(0, pending.clone()), (1, completed.clone())]).unwrap();
    let unknown = pending
        .ready(
            ResultId::new(pending.identity().id.as_str().into()).unwrap(),
            ExecutionOutcome::Indeterminate,
            StoredResult::unknown(UnknownResultReason::ResumeFence),
        )
        .unwrap();
    let resumed = RuntimeSnapshot::new(other_epoch(), vec![(1, unknown), (1, completed)]).unwrap();
    assert_eq!(
        RuntimeMutation::snapshot(resumed.clone(), SnapshotReason::ResumeFence)
            .unwrap()
            .apply(&branch(), Some((&branch(), &old)))
            .unwrap(),
        resumed
    );
    let fabricated = RuntimeSnapshot::new(
        other_epoch(),
        vec![(1, ready(&pending)), old.operations()[1].clone()],
    )
    .unwrap();
    assert!(
        RuntimeMutation::snapshot(fabricated, SnapshotReason::ResumeFence)
            .unwrap()
            .apply(&branch(), Some((&branch(), &old)))
            .is_err()
    );
    assert!(
        pending
            .ready(
                ResultId::new(pending.identity().id.as_str().into()).unwrap(),
                ExecutionOutcome::Indeterminate,
                StoredResult::inline(ResultEncoding::CodexResponseEnvelopeV1, "{}".into()).unwrap()
            )
            .is_err()
    );
}

#[test]
fn journal_snapshots_cannot_prune_or_rewrite_current_epoch_identity() {
    let completed = ready(&operation(10)).in_context(commit()).unwrap();
    let old = RuntimeSnapshot::new(epoch(), vec![(2, completed)]).unwrap();
    for reason in [SnapshotReason::Compaction, SnapshotReason::ReplayBound] {
        assert_eq!(
            RuntimeMutation::snapshot(old.clone(), reason)
                .unwrap()
                .apply(&branch(), Some((&branch(), &old)))
                .unwrap(),
            old
        );
        let pruned = RuntimeSnapshot::new(epoch(), Vec::new()).unwrap();
        assert!(
            RuntimeMutation::snapshot(pruned, reason)
                .unwrap()
                .apply(&branch(), Some((&branch(), &old)))
                .is_err()
        );
    }
}

#[test]
fn journal_serde_enforces_bounds_and_revisions_without_constructor_trust() {
    assert!(serde_json::from_value::<TurnId>(json!("é".repeat(65))).is_err());
    assert!(serde_json::from_value::<BranchId>(json!("not-a-uuid")).is_err());
    assert!(serde_json::from_value::<Digest>(json!("A".repeat(64))).is_err());
    assert!(serde_json::from_value::<BoundedJson>(json!("[]")).is_err());
    assert!(
        serde_json::from_value::<StoredResult>(
            json!({"Inline":{"encoding":"CodexResponseEnvelopeV1","payload":"x".repeat(4097)}})
        )
        .is_err()
    );
    assert!(
        StoredResult::artifact(
            ResultEncoding::CodexResponseEnvelopeV1,
            Digest::new("a".repeat(64)).unwrap(),
            16 * 1024 * 1024 + 1
        )
        .is_err()
    );
    let change = OperationChange::new(None, 0, operation(10)).unwrap();
    let mut invalid = serde_json::to_value(&change).unwrap();
    invalid["revision"] = json!(1);
    assert!(serde_json::from_value::<OperationChange>(invalid).is_err());
    assert!(
        serde_json::from_value::<RuntimeMutation>(json!({"Delta":{"changes":vec![change;33]}}))
            .is_err()
    );
    let duplicate = operation(10);
    assert!(RuntimeSnapshot::new(epoch(), vec![(0, duplicate.clone()), (0, duplicate)]).is_err());
    let operations: Vec<_> = (10..75).map(|index| (0, operation(index))).collect();
    assert!(
        serde_json::from_value::<RuntimeSnapshot>(
            json!({"execution_epoch":epoch(),"operations":operations})
        )
        .is_err()
    );
}

#[test]
fn journal_call_identity_cannot_be_reused_by_changing_only_the_turn() {
    let first = operation(10);
    let mut identity = first.identity().clone();
    identity.id = operation(11).identity().id.clone();
    identity.turn_id = TurnId::new("another-turn".into()).unwrap();
    let second = OperationRecord::prepared(identity, descriptor());
    assert!(RuntimeSnapshot::new(epoch(), vec![(0, first), (0, second)]).is_err());
}

#[test]
fn journal_call_identity_distinguishes_execution_epochs() {
    let original = operation(10);
    let completed = ready(&original);
    let mut identity = original.identity().clone();
    identity.id = operation(11).identity().id.clone();
    identity.owner_epoch = other_epoch();
    let current = OperationRecord::prepared(identity, descriptor());
    let snapshot = RuntimeSnapshot::new(
        other_epoch(),
        vec![(1, completed.clone()), (0, current.clone())],
    )
    .expect("same call and logical step in different epochs are distinct");
    assert_eq!(snapshot.operations(), &[(1, completed), (0, current)]);
}

#[test]
fn journal_fenced_anchor_rejects_current_epoch_prepared_without_prior_replay() {
    let prepared = RuntimeSnapshot::new(epoch(), vec![(0, operation(10))]).unwrap();
    for reason in [SnapshotReason::Fork, SnapshotReason::ResumeFence] {
        assert!(RuntimeMutation::snapshot(prepared.clone(), reason).is_err());
        let wire = json!({"Snapshot": {"state": prepared, "reason": reason}});
        assert!(serde_json::from_value::<RuntimeMutation>(wire).is_err());
    }
    // Ordinary checkpoints still represent active execution in their own epoch.
    for reason in [SnapshotReason::Compaction, SnapshotReason::ReplayBound] {
        let mutation = RuntimeMutation::snapshot(prepared.clone(), reason).unwrap();
        assert_eq!(
            serde_json::from_value::<RuntimeMutation>(serde_json::to_value(&mutation).unwrap())
                .unwrap(),
            mutation
        );
    }
}

#[test]
fn journal_checkpoint_rejects_unreachable_state_revisions_through_serde() {
    let cases = [
        (0, operation(10)),
        (1, ready(&operation(11))),
        (2, ready(&operation(12)).in_context(commit()).unwrap()),
        (1, operation(13).inherited()),
    ];
    for (revision, operation) in cases {
        let valid = RuntimeSnapshot::new(epoch(), vec![(revision, operation.clone())]).unwrap();
        for forged_revision in [0, 1, 2, 99]
            .into_iter()
            .filter(|forged| *forged != revision)
        {
            assert!(
                RuntimeSnapshot::new(epoch(), vec![(forged_revision, operation.clone())]).is_err()
            );
            let mut wire = serde_json::to_value(&valid).unwrap();
            wire["operations"][0][0] = json!(forged_revision);
            assert!(serde_json::from_value::<RuntimeSnapshot>(wire).is_err());
        }
    }
}

#[test]
fn journal_fork_and_resume_preserve_ready_and_delivered_revisions() {
    let operations = vec![
        (1, ready(&operation(10))),
        (2, ready(&operation(11)).in_context(commit()).unwrap()),
    ];
    let prior = RuntimeSnapshot::new(epoch(), operations.clone()).unwrap();
    let next = RuntimeSnapshot::new(other_epoch(), operations).unwrap();
    let child_branch = BranchId::new("00000000-0000-0000-0000-000000000099".into()).unwrap();
    for (reason, target) in [
        (SnapshotReason::Fork, child_branch),
        (SnapshotReason::ResumeFence, branch()),
    ] {
        let mutation = RuntimeMutation::snapshot(next.clone(), reason).unwrap();
        let restored: RuntimeMutation =
            serde_json::from_value(serde_json::to_value(mutation).unwrap()).unwrap();
        assert_eq!(
            restored.apply(&target, Some((&branch(), &prior))).unwrap(),
            next
        );
    }
}
