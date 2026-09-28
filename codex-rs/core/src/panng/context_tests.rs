use super::*;
use crate::session::startup::SessionStartup;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct ChangingContext(AtomicUsize);
impl ContextContributor for ChangingContext {
    fn contribute_world_state<'a>(
        &'a self,
        _: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        let revision = self.0.load(Ordering::SeqCst);
        Box::pin(async move {
            vec![WorldStateSectionContribution::new(
                "panng_context_test",
                json!(revision),
                move |previous| {
                    if matches!(previous, PreviousWorldStateSection::Known(value) if value == &json!(revision))
                    {
                        return None;
                    }
                    Some(RenderedWorldStateFragment::new(
                        "developer",
                        ("<panng_context_test>", "</panng_context_test>"),
                        format!("revision {revision}"),
                    ))
                },
            )]
        })
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    startup: Arc<SessionStartup>,
    session: Arc<Session>,
    context: Arc<ChangingContext>,
}

async fn fixture() -> anyhow::Result<Fixture> {
    let home = tempfile::tempdir()?;
    let codex_home = AbsolutePathBuf::try_from(home.path())?;
    let startup = Arc::new(SessionStartup::default());
    let context = Arc::new(ChangingContext(AtomicUsize::new(/*v*/ 1)));
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.prompt_contributor(context.clone());
    let (events, _receiver) = async_channel::unbounded();
    let session = crate::session::tests::make_session_with_setup_and_events(
        |config| {
            config.codex_home = codex_home;
            config.ephemeral = false;
            config.developer_instructions = Some("Panng accepted developer policy".into());
        },
        |_| {},
        ThreadHistoryMode::PanngV1,
        Some(Arc::clone(&startup)),
        events,
        Arc::new(extensions.build()),
    )
    .await?;
    Ok(Fixture {
        _home: home,
        startup,
        session,
        context,
    })
}

fn input() -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: Some(codex_protocol::ResponseItemId::from_server(
            "accepted-input".into(),
        )),
        role: "user".into(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: "Proceed with the accepted task".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

#[tokio::test]
async fn panng_step_commits_initial_then_incremental_context_with_input() -> anyhow::Result<()> {
    let fixture = fixture().await?;
    let session = &fixture.session;
    let step = StepContext::for_test(session.new_default_turn().await);
    let before = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    session
        .commit_panng_step(&step, vec![input()], Vec::new())
        .await?;
    let initial = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    assert_eq!(
        initial.commits.last().unwrap().sequence(),
        before.commits.last().unwrap().sequence() + 1
    );
    let effects = initial.commits.last().unwrap().host_effects();
    let world = effects
        .iter()
        .find_map(|item| match item {
            RolloutItem::WorldState(world) => Some(world),
            _ => None,
        })
        .unwrap();
    assert!(world.full);
    assert_eq!(world.state.get("panng_context_test"), Some(&json!(1)));
    let initial_history = session.clone_history().await;
    assert_eq!(
        initial_history.world_state_checkpoint(),
        Some(world.clone())
    );
    assert_eq!(
        initial_history.reference_context_item(),
        Some(step.to_turn_context_item())
    );
    let original_items = effects
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => Some(envelope.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(initial_history.annotated_items(), original_items);
    let encoded_context = serde_json::to_string(
        &original_items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect::<Vec<_>>(),
    )?;
    assert!(
        encoded_context.contains("Panng accepted developer policy"),
        "ordinary native developer context must be included"
    );
    assert!(encoded_context.contains("revision 1"));
    assert_eq!(
        original_items.last().unwrap().item.id().unwrap().as_str(),
        "accepted-input"
    );

    fixture.context.0.store(/*val*/ 2, Ordering::SeqCst);
    session
        .commit_panng_step(&step, Vec::new(), Vec::new())
        .await?;
    let refreshed = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    let refresh = refreshed.commits.last().unwrap();
    assert_eq!(
        refresh.sequence(),
        initial.commits.last().unwrap().sequence() + 1
    );
    let patch = refresh
        .host_effects()
        .iter()
        .find_map(|item| match item {
            RolloutItem::WorldState(world) => Some(world),
            _ => None,
        })
        .unwrap();
    assert!(!patch.full);
    assert_eq!(patch.state.get("panng_context_test"), Some(&json!(2)));
    let live = session.clone_history().await;
    assert!(
        live.annotated_items()
            .starts_with(initial_history.annotated_items()),
        "refresh must append instead of rewrite initial context"
    );
    assert_eq!(
        live.raw_items()
            .filter(|item| item.id().is_some_and(|id| id.as_str() == "accepted-input"))
            .count(),
        1
    );
    assert!(serde_json::to_string(&refresh.host_effects())?.contains("revision 2"));
    let mut expected_world = crate::context::world_state::WorldStateSnapshot::from(&world.state);
    expected_world.apply_merge_patch(&patch.state);
    assert_eq!(
        live.world_state_checkpoint(),
        Some(WorldStateItem::full(expected_world.into_object()))
    );

    session
        .commit_panng_step(&step, Vec::new(), Vec::new())
        .await?;
    assert_eq!(
        session
            .live_thread()
            .unwrap()
            .experimental_panng_read()
            .await?
            .cut,
        refreshed.cut,
        "unchanged empty refresh is a no-op"
    );
    fixture.startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_step_failed_commit_preserves_context_baseline_and_input() -> anyhow::Result<()> {
    let fixture = fixture().await?;
    let session = &fixture.session;
    let step = StepContext::for_test(session.new_default_turn().await);
    session
        .commit_panng_step(&step, vec![input()], Vec::new())
        .await?;
    let baseline = session.clone_history().await;
    fixture.context.0.store(/*val*/ 3, Ordering::SeqCst);
    session.live_thread().unwrap().shutdown().await?;
    assert!(
        session
            .commit_panng_step(&step, Vec::new(), Vec::new())
            .await
            .is_err()
    );
    let after = session.clone_history().await;
    assert_eq!(after.annotated_items(), baseline.annotated_items());
    assert_eq!(after.retained_context(), baseline.retained_context());
    assert_eq!(
        after.reference_context_item(),
        baseline.reference_context_item()
    );
    assert_eq!(
        after.world_state_checkpoint(),
        baseline.world_state_checkpoint()
    );
    fixture.startup.cleanup().await;
    Ok(())
}
