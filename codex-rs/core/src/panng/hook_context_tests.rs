use super::*;
use crate::session::startup::SessionStartup;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn panng_hook_context_ack_failure_preserves_live_history_and_returns_error()
-> anyhow::Result<()> {
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
    let turn = session.new_default_turn().await;
    record_additional_contexts_checked(&session, &turn, vec!["ACKNOWLEDGED_HOOK_CONTEXT".into()])
        .await?;
    let stored = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    assert!(serde_json::to_string(&stored.commits)?.contains("ACKNOWLEDGED_HOOK_CONTEXT"));
    let before = session.clone_history().await;
    session.live_thread().unwrap().shutdown().await?;
    assert!(
        record_additional_contexts_checked(
            &session,
            &turn,
            vec!["UNACKNOWLEDGED_HOOK_CONTEXT".into()]
        )
        .await
        .is_err()
    );
    let after = session.clone_history().await;
    assert_eq!(after.annotated_items(), before.annotated_items());
    assert_eq!(after.retained_context(), before.retained_context());
    startup.cleanup().await;
    Ok(())
}

#[test]
fn panng_hook_fragment_bound_includes_json_escaping() {
    assert!(panng_hook_context_items(vec!["\\".repeat(3000)]).is_err());
    assert!(panng_hook_context_items(vec!["small".into(); 17]).is_err());
    let items = panng_hook_context_items(vec!["bounded native hook context".into()]).unwrap();
    assert_eq!(items.len(), 1);
    assert!(
        serde_json::to_string(&items[0].item)
            .unwrap()
            .contains("bounded native hook context")
    );
}
