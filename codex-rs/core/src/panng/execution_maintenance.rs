//! Between-step maintenance reuses delivery and leaves the execution owner open.
use super::*;

enum InputWake {
    Allow,
    Ignore,
}

impl DurablePool {
    pub(crate) async fn advance(
        &self,
        session: &Arc<Session>,
        cancellation: &CancellationToken,
    ) -> Result<bool, CodexErr> {
        self.advance_inner(session, cancellation, InputWake::Allow)
            .await
    }

    async fn advance_inner(
        &self,
        session: &Arc<Session>,
        cancellation: &CancellationToken,
        input_wake: InputWake,
    ) -> Result<bool, CodexErr> {
        let _advance = self
            .advancement
            .try_acquire()
            .map_err(|_| fatal("concurrent durable advancement"))?;
        if let Some(error) = &self.state.lock().await.capture_error {
            return Err(fatal(error));
        }
        if self.cancellation.is_cancelled() || cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let (mut activity, _) = session
            .input_queue
            .subscribe_activity(/*turn_state*/ None)
            .await;
        if matches!(input_wake, InputWake::Allow)
            && session
                .input_queue
                .has_pending_input(&session.active_turn)
                .await
        {
            return Ok(true);
        }
        let (mut futures, has_observed) = {
            let mut state = self.state.lock().await;
            if let Some(error) = &state.capture_error {
                return Err(fatal(error));
            }
            (
                std::mem::take(&mut state.futures),
                !state.observed.is_empty(),
            )
        };
        if futures.is_empty() && !has_observed {
            return Ok(false);
        }
        let mut ready = Vec::new();
        if !has_observed && !futures.is_empty() {
            let first = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(CodexErr::TurnAborted),
                _ = self.cancellation.cancelled() => Err(CodexErr::TurnAborted),
                _ = activity.changed(), if matches!(input_wake, InputWake::Allow) => Ok(None),
                item = futures.next() => Ok(item),
            };
            match first {
                Ok(Some(item)) => ready.push(item),
                Ok(None) => {}
                Err(error) => {
                    self.state.lock().await.futures = futures;
                    return Err(error);
                }
            }
        }
        while let Some(Some(item)) = futures.next().now_or_never() {
            ready.push(item);
        }
        let ids = {
            let mut state = self.state.lock().await;
            state.futures = futures;
            state.observed.extend(ready);
            state.observed.keys().cloned().collect::<Vec<_>>()
        };
        for id in ids {
            if self.deliver(session, &id).await? {
                return Err(fatal(
                    "probe execution failed; its unknown outcome was recorded",
                ));
            }
        }
        Ok(true)
    }

    pub(crate) async fn drain(
        &self,
        session: &Arc<Session>,
        cancellation: &CancellationToken,
    ) -> Result<(), CodexErr> {
        while self
            .advance_inner(session, cancellation, InputWake::Ignore)
            .await?
        {}
        Ok(())
    }

    pub(crate) async fn maintain_epoch(
        &self,
        session: &Arc<Session>,
        step: &StepContext,
        cancellation: &CancellationToken,
    ) -> Result<(), CodexErr> {
        let runtime = session.panng_runtime_snapshot(&step.turn).await?;
        // ResumeFence preserves completed sources and records uncertain old executions as Ready.
        // Deliver them once; no executor is constructed or inherited.
        for (_, operation) in runtime.operations() {
            if matches!(operation.state(), OperationState::InheritedUnknown) {
                if cancellation.is_cancelled() {
                    return Err(CodexErr::TurnAborted);
                }
                let notice = crate::context::PanngCompletion::new(operation.identity().id.as_str().into(),
                    "This operation was inherited from the parent. It was not restarted in this fork; its outcome is unknown.".into())?;
                let envelope =
                    ResponseItemEnvelope::new(crate::context::ContextualUserFragment::into(notice));
                let transaction = session
                    .begin_panng_history(&step.turn, &step.settings.model_info, vec![envelope])
                    .await?;
                let (revision, current) =
                    find_operation(transaction.runtime()?, &operation.identity().id)?;
                let delivered = current
                    .inherited_in_context(
                        ResultId::new(uuid::Uuid::new_v4().to_string()).map_err(fatal)?,
                        transaction.commit_id().clone(),
                    )
                    .map_err(fatal)?;
                transaction
                    .commit(
                        RuntimeMutation::delta(vec![
                            OperationChange::new(Some(revision), revision + 1, delivered)
                                .map_err(fatal)?,
                        ])
                        .map_err(fatal)?,
                    )
                    .await?;
            }
            if matches!(operation.state(), OperationState::Ready { .. }) {
                if cancellation.is_cancelled() {
                    return Err(CodexErr::TurnAborted);
                }
                let completion =
                    completion_envelope(step.turn.config.codex_home.as_path(), operation).await?;
                let transaction = session
                    .begin_panng_history(&step.turn, &step.settings.model_info, vec![completion])
                    .await?;
                let (revision, current) =
                    find_operation(transaction.runtime()?, &operation.identity().id)?;
                let delivered = current
                    .in_context(transaction.commit_id().clone())
                    .map_err(fatal)?;
                transaction
                    .commit(
                        RuntimeMutation::delta(vec![
                            OperationChange::new(Some(revision), revision + 1, delivered)
                                .map_err(fatal)?,
                        ])
                        .map_err(fatal)?,
                    )
                    .await?;
            }
        }
        if runtime.operations().len() >= 48 {
            self.drain(session, cancellation).await?;
            session.retire_panng_delivered(&step.turn).await?;
        }
        Ok(())
    }
}
