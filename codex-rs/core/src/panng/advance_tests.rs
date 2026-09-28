use super::Pool;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use codex_history::ResponseItemEnvelope;
use codex_panng_runtime::Decision;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn panng_stop_wakes_pending_advance_without_restoring_closed_work() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let step = StepContext::for_test(Arc::new(turn));
    let pool = Pool::new();
    let cancellation = CancellationToken::new();
    let started = Arc::new(Notify::new());
    let executor_started = Arc::clone(&started);
    let (drop_signal, mut dropped) = oneshot::channel::<()>();
    pool.submit("pending".into(), step, &cancellation, || {
        Box::pin(async move {
            let _drop_signal = drop_signal;
            executor_started.notify_one();
            std::future::pending().await
        })
    })
    .await
    .expect("admission");
    let advance = pool.advance(&session, &cancellation);
    tokio::pin!(advance);
    tokio::select! {
        _ = started.notified() => {},
        result = &mut advance => panic!("advance ended before stop: {result:?}"),
    }
    // The first coordinator owns the futures locally; a second must not report idle.
    assert!(pool.advance(&session, &cancellation).await.is_err());
    let (closed, result) = tokio::time::timeout(Duration::from_secs(/*secs*/ 1), async {
        tokio::join!(
            async {
                pool.stop().await.expect("stop fence");
                // stop only returns after advancement has released ownership, including
                // any publication await. The pending executor has already been dropped.
                assert!(matches!(
                    dropped.try_recv(),
                    Err(oneshot::error::TryRecvError::Closed)
                ));
                assert_eq!(pool.advancement.available_permits(), 1);
                pool.state.lock().await.machine.clone()
            },
            &mut advance,
        )
    })
    .await
    .expect("pool cancellation wakes advancement and releases the stop fence");
    let error = result.expect_err("stopped turn");
    assert!(matches!(error.details(), CodexErrorDetails::TurnAborted));
    let state = pool.state.lock().await;
    assert_eq!(state.machine, closed);
    assert_eq!(state.machine.decide(), Decision::Closed);
    assert!(state.pending.is_empty());
    assert!(state.futures.is_empty());
}

#[tokio::test]
async fn panng_advance_publishes_success_before_a_later_fatal_result() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let step = StepContext::for_test(Arc::new(turn));
    let pool = Pool::new();
    let cancellation = CancellationToken::new();
    let (release_b, wait_for_a) = oneshot::channel();
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some("a".into()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text("A actual result".into()),
        internal_chat_message_metadata_passthrough: None,
    };
    let output_items = [output];
    let (prepared, _) = session
        .prepare_conversation_items_for_history(
            &step.turn,
            &step.settings.model_info,
            &output_items,
        )
        .await;
    let output = prepared.into_owned().pop().expect("prepared output");
    let expected = serde_json::to_value(&output).expect("expected output");
    // Even if FuturesUnordered polls B first, B cannot become ready before A.
    pool.submit("b".into(), Arc::clone(&step), &cancellation, || {
        Box::pin(async move {
            wait_for_a.await.expect("A releases B");
            Err(CodexErr::Fatal("B failed".into()))
        })
    })
    .await
    .expect("B admitted");
    pool.submit("a".into(), Arc::clone(&step), &cancellation, || {
        Box::pin(async move {
            release_b.send(()).expect("B is alive");
            Ok(ResponseItemEnvelope::new(output))
        })
    })
    .await
    .expect("A admitted");
    pool.submit("pending".into(), step, &cancellation, || {
        Box::pin(std::future::pending())
    })
    .await
    .expect("pending admitted");

    let error = pool
        .advance(&session, &cancellation)
        .await
        .expect_err("B is fatal");
    assert!(matches!(error.details(), CodexErrorDetails::Fatal(message) if message == "B failed"));
    let history = session.clone_history().await;
    let outputs: Vec<_> = history
        .raw_items()
        .filter(|item| matches!(item, ResponseItem::FunctionCallOutput { .. }))
        .map(|item| serde_json::to_value(item).expect("history output"))
        .collect();
    assert_eq!(
        outputs,
        vec![expected],
        "A is published; no remaining call receives a running ACK"
    );
    pool.stop().await.expect("stop fence");
}
