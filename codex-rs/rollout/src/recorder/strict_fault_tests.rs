use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn dropped_ack_receiver_retries_same_live_commit_without_another_line() {
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
    // This public request is queued after the command whose actual ACK receiver was dropped.
    let receipt = recorder.append_panng(resume.clone()).await.unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 3);
    assert_eq!(receipt.end_byte_offset, bytes.len() as u64);
    assert_eq!(
        recorder.reconcile_panng(resume).await.unwrap(),
        ReconciledAppend::Durable(receipt)
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    recorder.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflicting_tail_or_extra_complete_line_preserves_corrupt_evidence() {
    use std::io::Seek;
    use std::io::Write;
    for extra_complete_line in [false, true] {
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
        let mut corruptor = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        if extra_complete_line {
            corruptor.seek(io::SeekFrom::End(0)).unwrap();
            corruptor.write_all(b"{}\n").unwrap();
        } else {
            corruptor
                .seek(io::SeekFrom::Start(recovered.cut.end_byte_offset))
                .unwrap();
            corruptor.write_all(b"[").unwrap();
        }
        corruptor.sync_all().unwrap();
        let evidence = std::fs::read(&path).unwrap();
        assert!(matches!(
            state.reconcile(&mut file, resume.clone()).await,
            Err(SyncRecordError::Uncertain(_))
        ));
        assert!(state.poisoned);
        assert!(state.barrier().is_err());
        assert!(state.append(&mut file, resume.clone()).await.is_err());
        assert!(state.reconcile(&mut file, resume).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), evidence);
        assert_eq!(state.runtime, recovered.runtime);
    }
}

#[tokio::test]
async fn reconcile_truncate_and_sync_faults_retain_pending_and_converge_on_retry() {
    for fault in [
        WriteStage::ReconcileBeforeTruncate,
        WriteStage::ReconcileAfterTruncate,
        WriteStage::ReconcileBeforeSync,
        WriteStage::ReconcileAfterSync,
    ] {
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
        state.fault = Some(WriteStage::PartialWrite);
        assert!(matches!(
            state.append(&mut file, resume.clone()).await,
            Err(SyncRecordError::Uncertain(_))
        ));
        state.fault = Some(fault);
        assert!(matches!(
            state.reconcile(&mut file, resume.clone()).await,
            Err(SyncRecordError::Uncertain(_))
        ));
        assert!(state.pending.is_some());
        assert!(state.needs_resume_fence);
        assert_eq!(state.runtime, recovered.runtime);
        assert!(state.barrier().is_err());
        assert!(state.append(&mut file, resume.clone()).await.is_err());
        assert_eq!(
            state.reconcile(&mut file, resume.clone()).await.unwrap(),
            ReconciledAppend::Absent
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            recovered.cut.end_byte_offset
        );
        assert!(state.needs_resume_fence);
        assert_eq!(state.runtime, recovered.runtime);
        state.append(&mut file, resume).await.unwrap();
        assert!(!state.needs_resume_fence);
        assert_eq!(
            read_panng_suffix(std::fs::File::open(&path).unwrap())
                .unwrap()
                .runtime,
            state.runtime
        );
    }
}
