use super::*;
use crate::session::startup::SessionStartup;
use codex_history::CodexHarnessMetadata;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;

struct Fixture {
    _home: tempfile::TempDir,
    startup: Arc<SessionStartup>,
    session: Arc<Session>,
    events: async_channel::Receiver<Event>,
}

async fn fixture() -> anyhow::Result<Fixture> {
    let home = tempfile::tempdir()?;
    let startup = Arc::new(SessionStartup::default());
    let (tx, events) = async_channel::unbounded();
    let session = crate::session::tests::make_session_with_setup_and_events(
        |config| {
            config.codex_home = AbsolutePathBuf::try_from(home.path()).unwrap();
            config.ephemeral = false;
        },
        |_| {},
        ThreadHistoryMode::PanngV1,
        Some(Arc::clone(&startup)),
        tx,
        Arc::new(codex_extension_api::ExtensionRegistryBuilder::new().build()),
    )
    .await?;
    Ok(Fixture {
        _home: home,
        startup,
        session,
        events,
    })
}

fn message() -> ResponseItemEnvelope {
    ResponseItemEnvelope {
        item: serde_json::from_value(json!({
            "type":"message", "id":"source-message", "role":"assistant",
            "content":[{"type":"output_text","text":"A preserved source"}]
        }))
        .unwrap(),
        metadata: Some(CodexHarnessMetadata {
            client_authored: true,
            ..Default::default()
        }),
    }
}

fn raw_events(events: &async_channel::Receiver<Event>) -> Vec<ResponseItem> {
    let mut result = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::RawResponseItem(event) = event.msg {
            result.push(event.item);
        }
    }
    result
}

#[tokio::test]
async fn panng_provider_empty_reasoning_content_keeps_encrypted_payload_across_ack()
-> anyhow::Result<()> {
    let fixture = fixture().await?;
    let session = &fixture.session;
    let turn = session.new_default_turn().await;
    let item: ResponseItem = serde_json::from_value(json!({
        "type":"reasoning", "id":"provider-reasoning", "summary":[], "content":[],
        "encrypted_content":"opaque-provider-state"
    }))?;
    session
        .begin_panng_history(
            &turn,
            turn.model_info(),
            vec![ResponseItemEnvelope::new(item)],
        )
        .await?
        .commit(RuntimeMutation::delta(Vec::new())?)
        .await?;
    let recovered = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    let persisted = recovered
        .commits
        .last()
        .unwrap()
        .host_effects()
        .iter()
        .find_map(|effect| {
            if let RolloutItem::ResponseItem(envelope) = effect {
                Some(envelope)
            } else {
                None
            }
        })
        .unwrap();
    let ResponseItem::Reasoning {
        content,
        encrypted_content,
        ..
    } = &persisted.item
    else {
        panic!("reasoning source must retain its native family");
    };
    assert_eq!(
        (content, encrypted_content.as_deref()),
        (&None, Some("opaque-provider-state"))
    );
    assert_eq!(
        session.clone_history().await.annotated_items().last(),
        Some(persisted)
    );
    fixture.startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_host_history_ack_installs_replayable_envelopes_and_baselines() -> anyhow::Result<()>
{
    let fixture = fixture().await?;
    let session = &fixture.session;
    let turn = session.new_default_turn().await;
    let source = message();
    session
        .reserve_assistant_message_order(&turn, &source.item)
        .await;
    let baseline = session.clone_history().await;
    let output = ResponseItemEnvelope {
        item: serde_json::from_value(
            json!({"type":"function_call_output","id":"output-source","call_id":"call-source","output":"payload ".repeat(300)}),
        )?,
        metadata: Some(CodexHarnessMetadata {
            history_truncation_token_limit: Some(32),
            ..Default::default()
        }),
    };
    raw_events(&fixture.events);
    // A duplicate/no-op inspection releases its unprepared lease without fencing the session.
    session
        .begin_panng_history(&turn, turn.model_info(), Vec::new())
        .await?
        .cancel_unprepared()?;
    let transaction = session
        .begin_panng_history(&turn, turn.model_info(), vec![source, output])
        .await?;
    let commit_id = transaction.commit_id().clone();
    assert!(transaction.runtime()?.operations().is_empty());
    assert!(raw_events(&fixture.events).is_empty());
    assert_eq!(
        session.clone_history().await.annotated_items(),
        baseline.annotated_items()
    );
    let world = WorldStateItem::full(serde_json::from_value(json!({"host_test":{"ready":true}}))?);
    let context = turn.to_turn_context_item();
    let receipt = transaction
        .commit_with_effects(
            RuntimeMutation::delta(Vec::new())?,
            vec![
                RolloutItem::WorldState(world.clone()),
                RolloutItem::TurnContext(context.clone()),
            ],
        )
        .await?;
    let canonical = session
        .live_thread()
        .unwrap()
        .experimental_panng_read()
        .await?;
    let commit = canonical.commits.last().unwrap();
    assert_eq!(commit.commit_id(), &commit_id);
    assert_eq!(canonical.cut.end_byte_offset, receipt.end.end_byte_offset);
    let persisted = commit
        .host_effects()
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => Some(envelope.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut expected = baseline;
    for envelope in &persisted {
        expected.replay_annotated_item(envelope, turn.model_info().truncation_policy.into());
    }
    let installed = session.clone_history().await;
    assert_eq!(installed.annotated_items(), expected.annotated_items());
    assert_eq!(installed.retained_context(), expected.retained_context());
    assert_ne!(
        installed.annotated_items()[1].item,
        persisted[1].item,
        "live truncation must preserve the complete canonical original"
    );
    assert_eq!(installed.reference_context_item(), Some(context));
    assert_eq!(installed.world_state_checkpoint(), Some(world));
    assert_eq!(
        raw_events(&fixture.events),
        persisted
            .into_iter()
            .map(|envelope| envelope.item)
            .collect::<Vec<_>>()
    );
    assert!(
        turn.extension_data
            .get::<retained_context::PendingAssistantMessageOrders>()
            .unwrap()
            .0
            .lock()
            .unwrap()
            .is_empty()
    );
    fixture.startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_host_history_failure_preserves_live_history_and_pending_order() -> anyhow::Result<()>
{
    let fixture = fixture().await?;
    let session = &fixture.session;
    let turn = session.new_default_turn().await;
    let source = message();
    session
        .reserve_assistant_message_order(&turn, &source.item)
        .await;
    let baseline = session.clone_history().await;
    let orders = turn
        .extension_data
        .get::<retained_context::PendingAssistantMessageOrders>()
        .unwrap();
    let before_orders = orders.0.lock().unwrap().clone();
    raw_events(&fixture.events);
    let transaction = session
        .begin_panng_history(&turn, turn.model_info(), vec![source])
        .await?;
    session.live_thread().unwrap().shutdown().await?;
    assert!(
        transaction
            .commit(RuntimeMutation::delta(Vec::new())?)
            .await
            .is_err()
    );
    let after = session.clone_history().await;
    assert_eq!(after.annotated_items(), baseline.annotated_items());
    assert_eq!(after.retained_context(), baseline.retained_context());
    assert_eq!(*orders.0.lock().unwrap(), before_orders);
    assert!(raw_events(&fixture.events).is_empty());
    fixture.startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_host_history_rejects_an_uncoordinated_input_reservation() -> anyhow::Result<()> {
    let fixture = fixture().await?;
    let session = &fixture.session;
    let turn = session.new_default_turn().await;
    let transaction = session
        .begin_panng_history(&turn, turn.model_info(), vec![message()])
        .await?;
    session.reserve_user_input_order().await;
    let changed = session.clone_history().await;
    raw_events(&fixture.events);
    assert!(
        transaction
            .commit(RuntimeMutation::delta(Vec::new())?)
            .await
            .is_err()
    );
    let after = session.clone_history().await;
    assert_eq!(after.annotated_items(), changed.annotated_items());
    assert_eq!(after.retained_context(), changed.retained_context());
    assert!(raw_events(&fixture.events).is_empty());
    fixture.startup.cleanup().await;
    Ok(())
}
