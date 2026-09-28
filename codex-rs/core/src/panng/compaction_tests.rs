use super::*;
use crate::session::startup::SessionStartup;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn compact_candidate_changes_history_and_window_only_after_joint_ack() -> anyhow::Result<()> {
    for close_writer in [false, true] {
        let home = tempfile::tempdir()?;
        let startup = Arc::new(SessionStartup::default());
        let (events, _receiver) = async_channel::unbounded();
        let session = crate::session::tests::make_session_with_setup_and_events(
            |config| {
                config.codex_home = AbsolutePathBuf::try_from(home.path()).unwrap();
                config.ephemeral = false;
            },
            |_| {},
            ThreadHistoryMode::PanngV1,
            Some(startup.clone()),
            events,
            Arc::new(codex_extension_api::ExtensionRegistryBuilder::new().build()),
        )
        .await?;
        let step = StepContext::for_test(session.new_default_turn().await);
        session
            .commit_panng_step(&step, Vec::new(), Vec::new())
            .await?;
        let baseline = session.clone_history().await;
        let (old_number, old_ids, old_prefill) = {
            let state = session.state.lock().await;
            (
                state.auto_compact_window_number(),
                state.auto_compact_window_ids(),
                state.auto_compact_window_snapshot(),
            )
        };
        let (window_number, window_ids) = session.prepare_compaction_window().await;
        {
            let state = session.state.lock().await;
            assert_eq!(
                (
                    state.auto_compact_window_number(),
                    state.auto_compact_window_ids(),
                    state.auto_compact_window_snapshot()
                ),
                (old_number, old_ids, old_prefill)
            );
        }
        let summary = ResponseItemEnvelope::new(serde_json::from_value(serde_json::json!({
            "type":"message", "role":"assistant", "content":[{"type":"output_text","text":"retained summary"}]
        }))?);
        if close_writer {
            session.live_thread().unwrap().shutdown().await?;
        }
        let result = session
            .replace_compacted_history_checked(
                vec![summary],
                None,
                None,
                CompactedHistoryMetadata {
                    message: "summary".into(),
                    window_number,
                    window_ids,
                    compaction_response_id: Some("compact-response".into()),
                    compaction_model_hash: None,
                    reviewer_compaction_hash: None,
                },
            )
            .await;
        if close_writer {
            assert!(result.is_err());
            let state = session.state.lock().await;
            assert_eq!(
                (
                    state.auto_compact_window_number(),
                    state.auto_compact_window_ids(),
                    state.auto_compact_window_snapshot()
                ),
                (old_number, old_ids, old_prefill)
            );
            assert_eq!(state.history.annotated_items(), baseline.annotated_items());
            assert_eq!(
                state.history.world_state_checkpoint(),
                baseline.world_state_checkpoint()
            );
            assert_eq!(
                state.history.retained_context(),
                baseline.retained_context()
            );
        } else {
            result?;
            let live = session.clone_history().await;
            assert_eq!(live.annotated_items().len(), 1);
            let recovered = session
                .live_thread()
                .unwrap()
                .experimental_panng_read()
                .await?;
            let commit = recovered.commits.last().unwrap();
            assert_eq!(
                commit.mutation().snapshot_reason(),
                Some(SnapshotReason::Compaction)
            );
            let checkpoint = commit.checkpoint().unwrap();
            assert_eq!(
                checkpoint.compacted.replacement_history.as_deref(),
                Some(live.annotated_items())
            );
            assert_eq!(checkpoint.compacted.window_number, Some(window_number));
            assert_eq!(
                checkpoint.compacted.window_id,
                Some(window_ids.window_id.to_string())
            );
            let state = session.state.lock().await;
            assert_eq!(
                (
                    state.auto_compact_window_number(),
                    state.auto_compact_window_ids()
                ),
                (window_number, window_ids)
            );
        }
        startup.cleanup().await;
    }
    Ok(())
}
