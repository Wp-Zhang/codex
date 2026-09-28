use super::*;

impl RolloutRecorder {
    pub(super) fn prepare_create_state(
        config: &impl RolloutConfigView,
        params: RolloutRecorderParams,
    ) -> std::io::Result<RolloutWriterState> {
        let cwd = config.cwd().to_path_buf();
        match params {
            RolloutRecorderParams::Create {
                session_id,
                conversation_id,
                rollout_id_override,
                forked_from_id,
                forked_from_ordinal_exclusive,
                parent_thread_id,
                source,
                thread_source,
                originator,
                creator_user_id,
                creator_account_id,
                base_instructions,
                dynamic_tools,
                selected_capability_roots,
                runtime_workspace_roots,
                multi_agent_version,
                history_mode,
                history_base,
                subagent_history_start_ordinal,
                initial_window_id,
            } => {
                let ordinal_state =
                    RolloutOrdinalState::for_new_rollout(history_mode, history_base);
                let (path, started_at) =
                    precompute_new_rollout_path(config, conversation_id, rollout_id_override)?;

                let timestamp_format: &[FormatItem] = format_description!(
                    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
                );
                let timestamp = started_at
                    .to_offset(time::UtcOffset::UTC)
                    .format(timestamp_format)
                    .map_err(|e| IoError::other(format!("failed to format timestamp: {e}")))?;

                let session_meta = SessionMeta {
                    session_id,
                    id: conversation_id,
                    forked_from_id,
                    forked_from_ordinal_exclusive: forked_from_ordinal_exclusive
                        .filter(|_| forked_from_id.is_some()),
                    parent_thread_id,
                    timestamp,
                    cwd: cwd.clone(),
                    runtime_workspace_roots,
                    originator,
                    creator_user_id,
                    creator_account_id,
                    cli_version: env!("CARGO_PKG_VERSION").to_string(),
                    agent_nickname: source.get_nickname(),
                    agent_role: source.get_agent_role(),
                    agent_path: source.get_agent_path().map(Into::into),
                    source: *source,
                    thread_source,
                    model_provider: Some(config.model_provider_id().to_string()),
                    base_instructions: Some(base_instructions),
                    dynamic_tools: if dynamic_tools.is_empty() {
                        None
                    } else {
                        Some(dynamic_tools)
                    },
                    selected_capability_roots,
                    memory_mode: (!config.generate_memories()).then_some("disabled".to_string()),
                    history_mode,
                    history_base,
                    subagent_history_start_ordinal,
                    multi_agent_version,
                    context_window: initial_window_id.map(SessionContextWindow::new),
                };

                Ok(RolloutWriterState {
                    writer: None,
                    deferred_creation: true,
                    pending_items: Vec::new(),
                    meta: Some(session_meta),
                    cwd,
                    rollout_path: path,
                    ordinal_state,
                    last_logged_error: None,
                    synced: synced::SyncState::default(),
                    strict: None,
                })
            }
            RolloutRecorderParams::Resume { .. } => Err(IoError::new(
                std::io::ErrorKind::InvalidInput,
                "creation requires new-thread parameters",
            )),
        }
    }
}
