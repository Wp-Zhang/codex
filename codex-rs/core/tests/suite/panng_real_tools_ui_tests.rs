use super::*;
use codex_history::RolloutItem;
use codex_protocol::items::TurnItem;
use pretty_assertions::assert_eq;

pub(super) fn assert_canonical_tool_items(history: &codex_rollout::RecoveredPanngSuffix) {
    let mut actual = history
        .commits
        .iter()
        .flat_map(codex_history::PanngCommit::host_effects)
        .filter_map(|effect| match effect {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if matches!(
                    event.item,
                    TurnItem::CommandExecution(_) | TurnItem::FileChange(_)
                ) =>
            {
                Some(event.item.id())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut expected = history
        .runtime
        .operations()
        .iter()
        .map(|(_, operation)| operation.identity().call_id.as_str().to_string())
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(
        actual, expected,
        "each real tool has exactly one canonical terminal UI item"
    );
}

pub(super) async fn ui_items(
    test: &TestCodex,
) -> anyhow::Result<Vec<codex_thread_store::StoredThreadItem>> {
    let mut items = Vec::new();
    let mut position = None;
    loop {
        let page = test
            .thread_store
            .list_items(codex_thread_store::ListItemsParams {
                thread_id: test.session_configured.thread_id,
                turn_id: None,
                include_archived: true,
                position,
                page_size: 1,
                sort_direction: codex_thread_store::SortDirection::Asc,
                sort_key: codex_thread_store::ItemSortKey::CreatedAtOrdinal,
                after_updated_at_ordinal: None,
            })
            .await?;
        items.extend(page.items);
        match page.next_cursor {
            Some(cursor) => position = Some(codex_thread_store::ListItemsPosition::Cursor(cursor)),
            None => return Ok(items),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_real_output_has_bounded_cold_ui_without_changing_retained_blob() -> anyhow::Result<()>
{
    // Native exec clamps the requested max_output_tokens to the model's output policy.
    // Set both limits explicitly so this fixture tests Panng display truncation independently.
    let harness = TestCodexHarness::with_auto_env_builder(builder().with_config(|config| {
        config.tool_output_token_limit = Some(100_000);
    }))
    .await?;
    let source = format!("{}RETAINED_TAIL_MARKER\n", "z".repeat(130 * 1024));
    harness.write_file("ui-large.txt", &source).await?;
    let command = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => "cat ui-large.txt",
        TestTargetOs::Windows => "Get-Content -Raw ui-large.txt",
    };
    core_test_support::responses::mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_function_call(
                    "ui-large",
                    "exec_command",
                    &json!({
                        "cmd":command,"login":false,"timeout_ms":30_000,"max_output_tokens":100_000,
                    })
                    .to_string(),
                ),
                ev_completed("large"),
            ]),
            sse(vec![
                ev_assistant_message("done", "done"),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = harness.test();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Read the large file.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    complete(test).await?;
    let history = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    assert_canonical_tool_items(&history);
    let event = history
        .commits
        .iter()
        .flat_map(codex_history::PanngCommit::host_effects)
        .find_map(|effect| match effect {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if matches!(event.item, TurnItem::CommandExecution(_)) =>
            {
                Some(event)
            }
            _ => None,
        })
        .expect("durable command UI snapshot");
    let encoded_ui = serde_json::to_vec(event)?;
    assert!(
        encoded_ui.len() <= 32 * 1024,
        "serialized UI must be independently bounded: {}",
        encoded_ui.len()
    );
    assert!(
        String::from_utf8(encoded_ui)?
            .to_lowercase()
            .contains("omitted"),
        "display truncation must be explicit"
    );
    let (_, operation) = history.runtime.operations().iter().next().unwrap();
    let OperationState::InContext { result, .. } = operation.state() else {
        panic!("result not delivered")
    };
    let before = codex_rollout::load_panng_result_blob(test.codex_home_path(), result).await?;
    let RolloutItem::ResponseItem(envelope) = serde_json::from_slice(&before)? else {
        panic!("stored envelope")
    };
    let codex_protocol::models::ResponseItem::FunctionCallOutput { output, .. } = envelope.item
    else {
        panic!("native output")
    };
    // This fixture stays under native capture and explicitly requests a large model-output cap.
    // It does not claim Panng preserves bytes that native capture itself omitted.
    let retained = output.text_content().unwrap();
    assert!(
        !retained.contains("Warning: truncated output"),
        "native model-output policy unexpectedly truncated the fixture"
    );
    assert!(
        retained.contains(&source),
        "the full captured source must remain in the result blob"
    );
    let live_ui = ui_items(test).await?;
    test.codex.shutdown_and_wait().await?;
    assert_eq!(ui_items(test).await?, live_ui);
    assert_eq!(
        codex_rollout::load_panng_result_blob(test.codex_home_path(), result).await?,
        before
    );
    Ok(())
}
