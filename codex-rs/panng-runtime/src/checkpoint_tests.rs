use crate::Checkpoint;
use crate::Decision;
use crate::Kind;
use crate::Machine;
use crate::Status;
use pretty_assertions::assert_eq;
use std::io;
use std::io::Write;

#[test]
fn every_torn_checkpoint_boundary_recovers_the_previous_joint_state() {
    let mut machine = Machine::new(/*epoch*/ 1);
    machine.admit("b", Kind::Job).unwrap();
    machine.append("original".into()).unwrap();
    let cut = machine.cut();
    let old = Checkpoint::new(machine.clone());
    let mut log = Vec::new();
    old.write(&mut log).unwrap();
    machine
        .append("hook arrived during compression".into())
        .unwrap();
    machine.compact(cut, "summary".into()).unwrap();
    let next = Checkpoint::new(machine);
    let mut frame = Vec::new();
    next.write(&mut frame).unwrap();
    for crash_at in 0..frame.len() {
        let bytes = [log.as_slice(), &frame[..crash_at]].concat();
        assert_eq!(
            Checkpoint::read_latest(&mut bytes.as_slice()).unwrap(),
            Some(Checkpoint::new(old.machine().clone())),
            "crash at {crash_at}"
        );
    }
    log.extend(frame);
    let restored = Checkpoint::read_latest(&mut log.as_slice())
        .unwrap()
        .unwrap();
    assert_eq!(restored, next);
    assert_eq!(restored.machine().status("b"), Some(&Status::Pending));
    assert_eq!(restored.machine().history().len(), 2);
}

#[test]
fn committed_result_can_be_recovered_before_delivery() {
    let mut machine = Machine::new(/*epoch*/ 1);
    machine.admit("b", Kind::Job).unwrap();
    machine.complete("b", /*epoch*/ 1, "done".into()).unwrap();
    let mut file = tempfile::tempfile().unwrap();
    let expected = Checkpoint::new(machine);
    expected.write(&mut file).unwrap();
    file.sync_all().unwrap();
    use std::io::Seek;
    file.rewind().unwrap();
    let restored = Checkpoint::read_latest(&mut file).unwrap().unwrap();
    assert_eq!(restored, expected);
    assert_eq!(
        restored.machine().decide(),
        Decision::Deliver(vec!["b".into()])
    );
}

struct FailedDisk;
impl Write for FailedDisk {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("injected disk failure"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn failed_commit_never_publishes_candidate_or_admits_execution() {
    let mut live = Machine::new(/*epoch*/ 1);
    let mut candidate = live.clone();
    candidate.admit("side-effect", Kind::Job).unwrap();
    let result = Checkpoint::commit(&mut live, candidate, |checkpoint| {
        checkpoint.write(&mut FailedDisk)
    });
    assert!(result.is_err());
    assert_eq!(live.status("side-effect"), None);
}
