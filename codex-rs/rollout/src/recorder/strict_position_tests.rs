use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn panng_cold_durable_position_does_not_append_or_authorize_execution() {
    let (_directory, path, config, guard) = setup();
    let original = std::fs::read(&path).unwrap();
    let (recorder, recovered) =
        RolloutRecorder::resume_panng_with_writer_lock(&config, path.clone(), guard, branch())
            .await
            .unwrap();
    let expected = SyncedRecord {
        ordinal: Some(recovered.cut.ordinal),
        end_byte_offset: recovered.cut.end_byte_offset,
    };
    assert_eq!(recorder.panng_durable_position().await.unwrap(), expected);
    let ordinary = commit(1, RuntimeMutation::delta(Vec::new()).unwrap());
    assert!(matches!(
        recorder.append_panng(ordinary).await,
        Err(SyncRecordError::DefinitelyRejected(_))
    ));
    assert!(
        recorder
            .reconcile_panng(snapshot(0, initial_runtime()))
            .await
            .is_err(),
        "position query must not restore old attempt identity"
    );
    assert_eq!(recorder.panng_durable_position().await.unwrap(), expected);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let after = read_panng_suffix(std::fs::File::open(&path).unwrap()).unwrap();
    assert_eq!(after.runtime, recovered.runtime);
    recorder.shutdown().await.unwrap();
}

#[tokio::test]
async fn panng_durable_position_is_ordered_after_an_append_with_a_dropped_ack_receiver() {
    let (_directory, path, config, guard) = setup();
    let (recorder, recovered) =
        RolloutRecorder::resume_panng_with_writer_lock(&config, path.clone(), guard, branch())
            .await
            .unwrap();
    let resume = fence(1, &recovered.runtime, 3);
    let (ack, receiver) = oneshot::channel();
    drop(receiver);
    recorder
        .tx
        .send(RolloutCmd::AppendPanng {
            commit: Box::new(resume.clone()),
            ack,
        })
        .await
        .unwrap();
    let receipt = recorder.panng_durable_position().await.unwrap();
    assert_eq!(
        receipt,
        SyncedRecord {
            ordinal: Some(2),
            end_byte_offset: std::fs::metadata(&path).unwrap().len()
        }
    );
    assert_eq!(
        recorder.reconcile_panng(resume).await.unwrap(),
        ReconciledAppend::Durable(receipt)
    );
    assert_eq!(
        std::fs::read(&path)
            .unwrap()
            .iter()
            .filter(|byte| **byte == b'\n')
            .count(),
        3
    );
    recorder.shutdown().await.unwrap();
}

#[tokio::test]
async fn panng_durable_position_rejects_uncertain_and_poisoned_state() {
    use std::io::Write;
    let (_directory, path, _config, _guard) = setup();
    let recovered = read_panng_suffix(std::fs::File::open(&path).unwrap()).unwrap();
    let mut state = StrictState::recovered(&recovered).unwrap();
    let mut file = tokio::fs::File::from_std(
        std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap(),
    );
    let resume = fence(1, &recovered.runtime, 3);
    state.fault = Some(WriteStage::BeforeSync);
    assert!(matches!(
        state.append(&mut file, resume.clone()).await,
        Err(SyncRecordError::Uncertain(_))
    ));
    assert!(state.durable_position().is_err());
    let mut corruptor = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    corruptor.write_all(b"{}\n").unwrap();
    corruptor.sync_all().unwrap();
    assert!(state.reconcile(&mut file, resume).await.is_err());
    assert!(state.poisoned);
    assert!(state.durable_position().is_err());
}
