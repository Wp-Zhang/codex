//! Settings use native candidate validation, then the same durable lease as model history.
use super::*;
use codex_panng_runtime::RuntimeMutation;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;

impl Session {
    pub(crate) async fn update_panng_settings_if(
        &self,
        updates: SessionSettingsUpdate,
        should_commit: impl FnOnce(&SessionConfiguration, &SessionConfiguration) -> bool + Send,
    ) -> CodexResult<Option<SessionSettingsCommit>> {
        let _settings_guard = thread_settings::acquire_persistence_lock(self).await;
        let mut transaction = self.begin_panng_transaction().await?;
        let baseline = self.state.lock().await.session_configuration.clone();
        let candidate = self
            .apply_session_settings(&baseline, &updates)
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()));
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                transaction.cancel_unprepared().map_err(fatal)?;
                return Err(error);
            }
        };
        if !should_commit(&baseline, &candidate) {
            transaction.cancel_unprepared().map_err(fatal)?;
            return Ok(None);
        }
        let snapshot = candidate.thread_settings_snapshot(&candidate.environments);
        let previous_snapshot = baseline.thread_settings_snapshot(&baseline.environments);
        let commit = SessionSettingsCommit {
            configuration: candidate.clone(),
            snapshot: snapshot.clone(),
        };
        if snapshot == previous_snapshot
            && candidate.environments == baseline.environments
            && candidate.windows_sandbox_level == baseline.windows_sandbox_level
            && candidate.permission_profile() == baseline.permission_profile()
            && candidate.permission_profile_state.profile_workspace_roots()
                == baseline.permission_profile_state.profile_workspace_roots()
        {
            transaction.cancel_unprepared().map_err(fatal)?;
            return Ok(Some(commit));
        }
        let previous_config = (!self.services.extensions.config_contributors().is_empty())
            .then(|| self.build_effective_session_config(&baseline));
        let new_config = previous_config
            .as_ref()
            .map(|_| self.build_effective_session_config(&candidate));
        let permission_changed = baseline.permission_profile() != candidate.permission_profile();
        let mcp_changed = self.mcp_inputs_differ(&baseline, &candidate);
        let root_service_tier_changed = candidate.parent_thread_id.is_none()
            && baseline.step_settings.service_tier != candidate.step_settings.service_tier;
        let event = EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
            thread_id: Some(self.thread_id),
            thread_settings: snapshot,
        });
        transaction
            .prepare(
                RuntimeMutation::delta(Vec::new()).map_err(fatal)?,
                /*checkpoint*/ None,
                vec![RolloutItem::EventMsg(event)],
            )
            .map_err(fatal)?;
        let live = self
            .live_thread_for_persistence("update Panng thread settings")
            .map_err(fatal)?;
        transaction
            .commit_and_publish(live, |_| async move {
                {
                    let mut state = self.state.lock().await;
                    let current = &state.session_configuration;
                    if current.thread_settings_snapshot(&current.environments) != previous_snapshot
                        || current.environments != baseline.environments
                        || current.windows_sandbox_level != baseline.windows_sandbox_level
                        || current.permission_profile() != baseline.permission_profile()
                        || current.permission_profile_state.profile_workspace_roots()
                            != baseline.permission_profile_state.profile_workspace_roots()
                    {
                        anyhow::bail!("Panng settings changed outside the commit lease");
                    }
                    if mcp_changed {
                        self.mark_mcp_runtime_dirty();
                    }
                    state.session_configuration = candidate;
                    if root_service_tier_changed {
                        self.services.agent_control.propagate_config_update(
                            AgentConfigUpdate::ServiceTier(
                                state
                                    .session_configuration
                                    .step_settings
                                    .service_tier
                                    .clone(),
                            ),
                        );
                    }
                }
                self.emit_config_changed_contributors(
                    previous_config.as_ref(),
                    new_config.as_ref(),
                );
                if permission_changed {
                    self.refresh_managed_network_proxy_for_current_permission_profile()
                        .await;
                }
                if mcp_changed {
                    self.schedule_mcp_prewarm();
                }
                Ok(())
            })
            .await
            .map_err(fatal)?;
        Ok(Some(commit))
    }

    /// Settings callers publish after their reply/admission decision, never through native append.
    pub(crate) async fn emit_panng_settings_applied(
        &self,
        submission_id: String,
        snapshot: ThreadSettingsSnapshot,
    ) {
        let msg = EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
            thread_id: Some(self.thread_id),
            thread_settings: snapshot,
        });
        self.services.mcp_runtime.observe_event(&msg);
        self.services
            .rollout_thread_trace
            .record_protocol_event(&msg);
        self.deliver_event_raw(Event {
            id: submission_id,
            msg,
        })
        .await;
    }
}

fn fatal(error: impl std::fmt::Display) -> CodexErr {
    CodexErr::Fatal(error.to_string())
}
