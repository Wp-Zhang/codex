use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panng_bad_result_read_arguments_let_the_model_correct_and_continue() -> anyhow::Result<()>
{
    let harness = harness().await?;
    let command = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => "printf recovered > recovered.txt",
        TestTargetOs::Windows => "Set-Content -NoNewline recovered.txt recovered",
    };
    let mut sequence = vec![];
    for (call, arguments) in [
        ("invalid-json", "{"),
        ("invalid-limit", r#"{"operation_id":"missing","limit":0}"#),
        ("unknown-operation", r#"{"operation_id":"missing"}"#),
    ] {
        sequence.push(sse(vec![
            ev_function_call(call, "read_operation_result", arguments),
            ev_completed(call),
        ]));
    }
    sequence.push(sse(vec![exec("recover", command), ev_completed("recover")]));
    sequence.push(sse(vec![
        ev_assistant_message("done", "done"),
        ev_completed("done"),
    ]));
    let mock = core_test_support::responses::mount_sse_sequence(harness.server(), sequence).await;
    harness
        .test()
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Read a previous result and then write the recovery file.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    complete(harness.test()).await?;
    assert_eq!(harness.read_file_text("recovered.txt").await?, "recovered");
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    for request in &requests[1..] {
        assert!(request.body_contains_text("Result read rejected:"));
    }
    let test = harness.test();
    let history = test
        .thread_store
        .experimental_panng_read(test.session_configured.thread_id)
        .await?;
    assert_eq!(
        history.runtime.operations().len(),
        1,
        "rejected pure reads never allocate jobs"
    );
    resume_and_compare(test).await?;
    Ok(())
}
