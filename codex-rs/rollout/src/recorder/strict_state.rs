use super::*;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;

impl StrictState {
    pub(super) fn recovered(recovered: &RecoveredPanngSuffix) -> io::Result<Self> {
        let last = recovered
            .commits
            .last()
            .ok_or_else(|| io::Error::other("missing recovered commit"))?;
        Ok(Self {
            branch: recovered.branch.clone(),
            thread_id: recovered.thread_id,
            runtime: recovered.runtime.clone(),
            sequence: last.sequence(),
            ordinal: recovered.cut.ordinal,
            end: recovered.cut.end_byte_offset,
            needs_resume_fence: true,
            poisoned: false,
            pending: None,
            last: None,
            bytes_since_checkpoint: 0,
            records_since_checkpoint: 0,
            root_id: recovered.initial_commit_id.clone(),
            seen_ids: recovered
                .commits
                .iter()
                .map(|commit| commit.commit_id().clone())
                .chain(std::iter::once(recovered.initial_commit_id.clone()))
                .collect(),
            #[cfg(test)]
            fault: None,
        })
    }
    pub(in crate::recorder) fn barrier(&self) -> io::Result<()> {
        if self.pending.is_some() || self.poisoned {
            return Err(io::Error::other("strict writer needs reconciliation"));
        }
        Ok(())
    }
    fn fault(&mut self, stage: WriteStage) -> io::Result<()> {
        #[cfg(test)]
        if self.fault == Some(stage) {
            self.fault = None;
            return Err(io::Error::other("injected strict write failure"));
        }
        let _ = stage;
        Ok(())
    }
    fn retry(
        &self,
        commit: &PanngCommit,
        payload: &[u8],
    ) -> Result<Option<SyncedRecord>, SyncRecordError> {
        if let Some(last) = &self.last
            && &last.id == commit.commit_id()
        {
            if last.payload != payload {
                return Err(rejected("same commit identity has different payload"));
            }
            return Ok(Some(last.receipt.clone()));
        }
        Ok(None)
    }
    pub(super) async fn append(
        &mut self,
        file: &mut tokio::fs::File,
        commit: PanngCommit,
    ) -> Result<SyncedRecord, SyncRecordError> {
        self.barrier()
            .map_err(SyncRecordError::DefinitelyRejected)?;
        let payload = commit
            .canonical_bytes()
            .map_err(|error| rejected(error.to_string()))?;
        if file
            .metadata()
            .await
            .map_err(SyncRecordError::DefinitelyRejected)?
            .len()
            != self.end
        {
            self.poisoned = true;
            return Err(rejected("strict file length changed outside writer"));
        }
        if let Some(receipt) = self.retry(&commit, &payload)? {
            return Ok(receipt);
        }
        if self.seen_ids.contains(commit.commit_id()) {
            return Err(rejected(
                "commit identity already appears in inspected checkpoint scope",
            ));
        }
        if commit.branch() != &self.branch
            || self.sequence.checked_add(1) != Some(commit.sequence())
            || commit.previous_sequence() != Some(self.sequence)
        {
            return Err(rejected("stale commit, branch mismatch or sequence gap"));
        }
        if self.needs_resume_fence
            && commit.mutation().snapshot_reason() != Some(SnapshotReason::ResumeFence)
        {
            return Err(rejected("cold writer requires a new epoch ResumeFence"));
        }
        crate::panng_recovery::validate_thread_settings(&commit, self.thread_id)
            .map_err(SyncRecordError::DefinitelyRejected)?;
        let candidate = commit
            .mutation()
            .apply(&self.branch, Some((&self.branch, &self.runtime)))
            .map_err(|error| rejected(error.to_string()))?;
        let ordinal = self
            .ordinal
            .checked_add(1)
            .filter(|ordinal| *ordinal < u64::MAX)
            .ok_or_else(|| rejected("ordinal exhausted"))?;
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| rejected(error.to_string()))?;
        let mut line = serde_json::to_vec(&serde_json::json!({
            "type":"panng_commit", "timestamp":timestamp, "ordinal":ordinal, "payload":&commit,
        }))
        .map_err(|error| rejected(error.to_string()))?;
        line.push(b'\n');
        let checkpoint = commit.is_checkpoint();
        if !checkpoint
            && (self.records_since_checkpoint >= 255
                || self.bytes_since_checkpoint.saturating_add(line.len()) > 8 * 1024 * 1024)
        {
            return Err(rejected("joint checkpoint required before further deltas"));
        }
        self.end
            .checked_add(line.len() as u64)
            .ok_or_else(|| rejected("byte offset exhausted"))?;
        self.pending = Some(Attempt {
            id: commit.commit_id().clone(),
            payload,
            line,
            sequence: commit.sequence(),
            ordinal,
            candidate,
            checkpoint,
        });
        let result = async {
            self.fault(WriteStage::BeforeWrite)?;
            #[cfg(test)]
            if self.fault == Some(WriteStage::PartialWrite) {
                let line = &self.pending.as_ref().unwrap().line;
                file.write_all(&line[..line.len() / 2]).await?;
                file.flush().await?;
                self.fault(WriteStage::PartialWrite)?;
            }
            let Some(attempt) = &self.pending else {
                self.poisoned = true;
                return Err(io::Error::other("strict writer lost its pending attempt"));
            };
            file.write_all(&attempt.line).await?;
            file.flush().await?;
            self.fault(WriteStage::BeforeSync)?;
            file.sync_all().await?;
            self.fault(WriteStage::AfterSync)?;
            Ok::<_, io::Error>(())
        }
        .await;
        result.map_err(SyncRecordError::Uncertain)?;
        self.publish()
    }
    fn publish(&mut self) -> Result<SyncedRecord, SyncRecordError> {
        let Some(attempt) = self.pending.take() else {
            self.poisoned = true;
            return Err(uncertain("strict writer lost its acknowledged attempt"));
        };
        self.end += attempt.line.len() as u64;
        self.sequence = attempt.sequence;
        self.ordinal = attempt.ordinal;
        self.runtime = attempt.candidate;
        self.needs_resume_fence = false;
        if attempt.checkpoint {
            self.bytes_since_checkpoint = 0;
            self.records_since_checkpoint = 0;
            self.seen_ids.clear();
            self.seen_ids.insert(self.root_id.clone());
        } else {
            self.bytes_since_checkpoint += attempt.line.len();
            self.records_since_checkpoint += 1;
        }
        self.seen_ids.insert(attempt.id.clone());
        let receipt = SyncedRecord {
            ordinal: Some(self.ordinal),
            end_byte_offset: self.end,
        };
        self.last = Some(LastCommit {
            id: attempt.id,
            payload: attempt.payload,
            receipt: receipt.clone(),
        });
        Ok(receipt)
    }
    pub(super) async fn reconcile(
        &mut self,
        file: &mut tokio::fs::File,
        commit: PanngCommit,
    ) -> Result<ReconciledAppend, SyncRecordError> {
        if self.poisoned {
            return Err(rejected("strict writer is corrupt"));
        }
        let payload = commit
            .canonical_bytes()
            .map_err(|error| rejected(error.to_string()))?;
        let Some(attempt) = &self.pending else {
            if file
                .metadata()
                .await
                .map_err(SyncRecordError::Uncertain)?
                .len()
                != self.end
            {
                self.poisoned = true;
                return Err(uncertain("strict file length changed outside writer"));
            }
            return self
                .retry(&commit, &payload)?
                .map(ReconciledAppend::Durable)
                .ok_or_else(|| rejected("no matching live attempt; cold or stale retry"));
        };
        if &attempt.id != commit.commit_id() || attempt.payload != payload {
            return Err(rejected("reconcile differs from pending commit"));
        }
        file.flush().await.map_err(SyncRecordError::Uncertain)?;
        let len = file
            .metadata()
            .await
            .map_err(SyncRecordError::Uncertain)?
            .len();
        let size = len
            .checked_sub(self.end)
            .filter(|size| *size <= attempt.line.len() as u64);
        let Some(size) = size else {
            self.poisoned = true;
            return Err(uncertain("strict prefix shrank or extra tail appeared"));
        };
        let mut bytes = vec![0; size as usize];
        file.seek(io::SeekFrom::Start(self.end))
            .await
            .map_err(SyncRecordError::Uncertain)?;
        file.read_exact(&mut bytes)
            .await
            .map_err(SyncRecordError::Uncertain)?;
        if !attempt.line.starts_with(&bytes) {
            self.poisoned = true;
            return Err(uncertain("tail differs from exact attempted bytes"));
        }
        let complete = bytes.len() == attempt.line.len();
        if !complete {
            #[cfg(test)]
            self.fault(WriteStage::ReconcileBeforeTruncate)
                .map_err(SyncRecordError::Uncertain)?;
            file.set_len(self.end)
                .await
                .map_err(SyncRecordError::Uncertain)?;
            #[cfg(test)]
            self.fault(WriteStage::ReconcileAfterTruncate)
                .map_err(SyncRecordError::Uncertain)?;
        }
        #[cfg(test)]
        self.fault(WriteStage::ReconcileBeforeSync)
            .map_err(SyncRecordError::Uncertain)?;
        file.sync_all().await.map_err(SyncRecordError::Uncertain)?;
        #[cfg(test)]
        self.fault(WriteStage::ReconcileAfterSync)
            .map_err(SyncRecordError::Uncertain)?;
        if complete {
            self.publish().map(ReconciledAppend::Durable)
        } else {
            self.pending = None;
            Ok(ReconciledAppend::Absent)
        }
    }
}
