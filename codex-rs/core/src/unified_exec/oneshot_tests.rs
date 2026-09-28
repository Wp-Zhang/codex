use super::*;
use crate::session::tests::make_session_and_context_with_rx;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn one_shot_cleanup_waits_only_for_the_aborted_call() -> anyhow::Result<()> {
    let (_, turn, _) = make_session_and_context_with_rx().await;
    let cleanup = turn.extension_data.get_or_init(OneShotCleanup::default);
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let (second_tx, second_rx) = tokio::sync::oneshot::channel();
    cleanup
        .0
        .lock()
        .unwrap()
        .extend([("first".into(), first_rx), ("second".into(), second_rx)]);
    let mut joined = Box::pin(UnifiedExecProcessManager::join_one_shot_cleanup(
        &turn, "first",
    ));
    assert!(futures::poll!(joined.as_mut()).is_pending());
    first_tx
        .send(())
        .expect("task completed after UI publication");
    joined.await?;
    assert_eq!(
        cleanup
            .0
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["second".to_string()]
    );
    second_tx.send(()).expect("other task completed");
    UnifiedExecProcessManager::join_one_shot_cleanup(&turn, "second").await?;
    assert!(cleanup.0.lock().unwrap().is_empty());
    UnifiedExecProcessManager::join_one_shot_cleanup(&turn, "not-one-shot").await?;
    Ok(())
}

#[tokio::test]
async fn one_shot_cleanup_does_not_confirm_a_dropped_task() {
    let (_, turn, _) = make_session_and_context_with_rx().await;
    let cleanup = turn.extension_data.get_or_init(OneShotCleanup::default);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    cleanup.0.lock().unwrap().insert("call".into(), receiver);
    drop(sender);
    let error = UnifiedExecProcessManager::join_one_shot_cleanup(&turn, "call")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("without confirming completion"));
}
