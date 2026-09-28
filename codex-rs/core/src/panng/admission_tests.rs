use super::Pool;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug)]
enum Rejection {
    Duplicate,
    PendingLimit,
    LongId,
    EmptyId,
    Closing,
    PoolCancelled,
    TurnCancelled,
}

#[tokio::test]
async fn panng_admission_rejection_never_constructs_an_executor() {
    let (_session, turn) = make_session_and_context().await;
    let step = StepContext::for_test(Arc::new(turn));

    for rejection in [
        Rejection::Duplicate,
        Rejection::PendingLimit,
        Rejection::LongId,
        Rejection::EmptyId,
        Rejection::Closing,
        Rejection::PoolCancelled,
        Rejection::TurnCancelled,
    ] {
        let pool = Pool::new();
        let cancellation = CancellationToken::new();
        let id = match rejection {
            Rejection::Duplicate => {
                pool.submit("probe".into(), Arc::clone(&step), &cancellation, || {
                    Box::pin(std::future::pending())
                })
                .await
                .expect("first admission succeeds");
                "probe".to_string()
            }
            Rejection::PendingLimit => {
                for index in 0..64 {
                    pool.submit(
                        format!("probe-{index}"),
                        Arc::clone(&step),
                        &cancellation,
                        || Box::pin(std::future::pending()),
                    )
                    .await
                    .expect("admission within the limit succeeds");
                }
                "overflow".to_string()
            }
            Rejection::LongId => "x".repeat(129),
            Rejection::EmptyId => String::new(),
            Rejection::Closing => {
                pool.state.lock().await.machine.begin_close();
                "probe".to_string()
            }
            Rejection::PoolCancelled => {
                pool.cancellation.cancel();
                "probe".to_string()
            }
            Rejection::TurnCancelled => {
                cancellation.cancel();
                "probe".to_string()
            }
        };
        let before = pool.state.lock().await.machine.clone();
        let constructions = AtomicUsize::new(0);
        let result = pool
            .submit(id, Arc::clone(&step), &cancellation, || {
                constructions.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::pending())
            })
            .await;

        assert!(result.is_err(), "expected rejection for {rejection:?}");
        assert_eq!(constructions.load(Ordering::SeqCst), 0, "{rejection:?}");
        assert_eq!(pool.state.lock().await.machine, before, "{rejection:?}");
    }
}

#[tokio::test]
async fn panng_admission_constructs_once_without_polling_the_executor() {
    let (_session, turn) = make_session_and_context().await;
    let step = StepContext::for_test(Arc::new(turn));
    let pool = Pool::new();
    let cancellation = CancellationToken::new();
    let constructions = AtomicUsize::new(0);
    let polls = Arc::new(AtomicUsize::new(0));
    let executor_polls = Arc::clone(&polls);

    pool.submit("probe".into(), step, &cancellation, || {
        constructions.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            executor_polls.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        })
    })
    .await
    .expect("probe admission succeeds");

    // The factory can have eager effects even though its returned future has not
    // been polled. This verifies the in-memory boundary, not durable admission.
    assert_eq!(
        (
            constructions.load(Ordering::SeqCst),
            polls.load(Ordering::SeqCst)
        ),
        (1, 0)
    );
}
