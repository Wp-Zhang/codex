use crate::Decision;
use crate::Kind;
use crate::Machine;
use crate::Status;
use pretty_assertions::assert_eq;

#[test]
fn ready_work_advances_while_another_job_is_pending() {
    let mut machine = Machine::new(/*epoch*/ 1);
    machine.admit("a", Kind::Job).unwrap();
    machine.admit("b", Kind::Job).unwrap();
    assert_eq!(machine.decide(), Decision::Wait);
    machine
        .complete("a", /*epoch*/ 1, "result a".into())
        .unwrap();
    assert_eq!(machine.decide(), Decision::Deliver(vec!["a".into()]));
    assert_eq!(machine.deliver("a"), Ok("result a".into()));
    machine.admit("c", Kind::Job).unwrap();
    assert_eq!(machine.status("b"), Some(&Status::Pending));
    assert_eq!(machine.decide(), Decision::Wait);
    assert!(machine.deliver("a").is_err());
}

#[test]
fn compaction_keeps_hook_suffix_and_does_not_revoke_execution_epoch() {
    let mut machine = Machine::new(/*epoch*/ 7);
    machine.admit("b", Kind::Job).unwrap();
    machine.append("before".into()).unwrap();
    let cut = machine.cut();
    machine.append("hook context".into()).unwrap();
    machine.compact(cut, "summary".into()).unwrap();
    machine.complete("b", /*epoch*/ 7, "output".into()).unwrap();
    assert_eq!(
        machine
            .history()
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["summary", "hook context"]
    );
    assert_eq!(machine.deliver("b"), Ok("output".into()));
    assert!(machine.compact(cut, "stale".into()).is_err());
}

#[test]
fn close_is_idempotent_and_late_updates_cannot_reopen_admission() {
    let mut machine = Machine::new(/*epoch*/ 4);
    machine.admit("b", Kind::Job).unwrap();
    machine.begin_close();
    assert_eq!(machine.decide(), Decision::Settle);
    assert!(machine.admit("c", Kind::Job).is_err());
    machine.settle().unwrap();
    let closed = machine.clone();
    machine.begin_close();
    machine.settle().unwrap();
    assert_eq!(machine, closed);
    assert_eq!(machine.status("b"), Some(&Status::Indeterminate));
    assert!(machine.complete("b", /*epoch*/ 4, "late".into()).is_err());
    assert!(machine.append("late hook".into()).is_err());
    assert_eq!(machine.decide(), Decision::Closed);
}

#[test]
fn historical_fork_inherits_neither_pending_work_nor_resource_execution() {
    let mut parent = Machine::new(/*epoch*/ 10);
    parent.admit("job", Kind::Job).unwrap();
    parent.admit("server", Kind::Resource).unwrap();
    let at_cut = parent.clone();
    parent
        .complete("job", /*epoch*/ 10, "later".into())
        .unwrap();
    let mut child = at_cut.fork(/*epoch*/ 11).unwrap();
    assert_eq!(child.decide(), Decision::Idle);
    assert_eq!(child.status("job"), Some(&Status::Inherited));
    assert_eq!(child.status("server"), Some(&Status::Inherited));
    assert!(
        child
            .complete("job", /*epoch*/ 10, "parent result".into())
            .is_err()
    );
    assert_eq!(parent.decide(), Decision::Deliver(vec!["job".into()]));
}

#[test]
fn resources_do_not_block_idle_but_jobs_do() {
    let mut machine = Machine::new(/*epoch*/ 1);
    machine.admit("server", Kind::Resource).unwrap();
    assert_eq!(machine.decide(), Decision::Idle);
    machine.admit("stdin", Kind::Job).unwrap();
    assert_eq!(machine.decide(), Decision::Wait);
    machine
        .complete("stdin", /*epoch*/ 1, "sent".into())
        .unwrap();
    machine.deliver("stdin").unwrap();
    assert_eq!(machine.decide(), Decision::Idle);
}

#[test]
fn fork_preserves_ready_results_without_inheriting_later_parent_execution() {
    let mut parent = Machine::new(/*epoch*/ 10);
    parent.admit("ready", Kind::Job).unwrap();
    parent.admit("pending", Kind::Job).unwrap();
    parent
        .complete("ready", /*epoch*/ 10, "result at cut".into())
        .unwrap();
    let mut child = parent.fork(/*epoch*/ 11).unwrap();
    parent
        .complete("pending", /*epoch*/ 10, "after cut".into())
        .unwrap();
    assert_eq!(child.decide(), Decision::Deliver(vec!["ready".into()]));
    assert_eq!(child.deliver("ready"), Ok("result at cut".into()));
    assert_eq!(child.status("pending"), Some(&Status::Inherited));
    assert_eq!(child.decide(), Decision::Idle);
    assert_eq!(parent.deliver("ready"), Ok("result at cut".into()));
    assert_eq!(parent.deliver("pending"), Ok("after cut".into()));
}
