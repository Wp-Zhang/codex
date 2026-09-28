use super::*;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use pretty_assertions::assert_eq;

async fn fixture() -> anyhow::Result<(
    tempfile::TempDir,
    Arc<startup::SessionStartup>,
    Arc<Session>,
)> {
    let home = tempfile::tempdir()?;
    let startup = Arc::new(startup::SessionStartup::default());
    let (tx, _rx) = async_channel::unbounded();
    let session = crate::session::tests::make_session_with_setup_and_events(
        |config| {
            config.codex_home =
                codex_utils_absolute_path::AbsolutePathBuf::try_from(home.path()).unwrap();
            config.ephemeral = false;
            config.features.disable(Feature::CodeMode).unwrap();
        },
        |_| {},
        ThreadHistoryMode::PanngV1,
        Some(startup.clone()),
        tx,
        Arc::new(codex_extension_api::ExtensionRegistryBuilder::new().build()),
    )
    .await?;
    Ok((home, startup, session))
}

#[tokio::test]
async fn panng_admission_preserves_requested_settings_for_durable_installation()
-> anyhow::Result<()> {
    let (_home, startup, session) = fixture().await?;
    let before = session.thread_settings_snapshot().await;
    let mut collaboration = before.collaboration_mode.clone();
    collaboration.settings.developer_instructions =
        Some("TUI-selected default instructions".into());
    collaboration.settings.reasoning_effort = Some(ReasoningEffort::High);
    let request = TurnInputRequest::user_input(vec![UserInput::Text {
        text: "hello".into(),
        text_elements: Vec::new(),
    }])
    .with_thread_settings(ThreadSettingsOverrides {
        collaboration_mode: Some(collaboration),
        ..Default::default()
    });
    let requested = request.thread_settings.clone();
    validate(&session, &request, &TurnInputMode::StartIfIdle).await?;
    assert_eq!(request.thread_settings, requested);
    assert_eq!(session.thread_settings_snapshot().await, before);
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_settings_rejected_start_predicate_does_not_commit() -> anyhow::Result<()> {
    let (_home, startup, session) = fixture().await?;
    let before = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?
        .cut;
    let snapshot = session.thread_settings_snapshot().await;
    let changes = thread_settings::prepare_update(ThreadSettingsOverrides {
        effort: Some(Some(ReasoningEffort::High)),
        ..Default::default()
    });
    assert!(
        session
            .update_panng_settings_if(changes, |_, _| false)
            .await?
            .is_none()
    );
    assert_eq!(session.thread_settings_snapshot().await, snapshot);
    assert_eq!(
        session
            .live_thread()
            .unwrap()
            .experimental_panng_read()
            .await?
            .cut,
        before
    );
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_settings_failed_ack_leaves_installed_configuration_unchanged() -> anyhow::Result<()>
{
    let (_home, startup, session) = fixture().await?;
    let live = session.live_thread().unwrap();
    let path = live.local_rollout_path().await?.unwrap();
    let before = session.thread_settings_snapshot().await;
    live.shutdown().await?;
    let bytes = tokio::fs::read(&path).await?;
    let changes = thread_settings::prepare_update(ThreadSettingsOverrides {
        effort: Some(Some(ReasoningEffort::High)),
        ..Default::default()
    });
    assert!(
        session
            .update_panng_settings_if(changes, |_, _| true)
            .await
            .is_err()
    );
    assert_eq!(session.thread_settings_snapshot().await, before);
    assert_eq!(tokio::fs::read(&path).await?, bytes);
    startup.cleanup().await;
    Ok(())
}
