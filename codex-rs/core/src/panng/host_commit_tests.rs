use super::*;
use crate::session::panng_bootstrap::Bootstrap;
use crate::session::session::Session;
use crate::session::startup::SessionStartup;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

async fn fixture() -> anyhow::Result<(
    tempfile::TempDir,
    Arc<SessionStartup>,
    Arc<Session>,
    HostCommitCoordinator,
)> {
    let home = tempfile::tempdir()?;
    let startup = Arc::new(SessionStartup::default());
    let (tx, _rx) = async_channel::unbounded();
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
    let coordinator = session
        .services
        .thread_extension_data
        .get::<Bootstrap>()
        .unwrap()
        .coordinator()
        .clone();
    Ok((home, startup, session, coordinator))
}

#[tokio::test]
async fn panng_host_commit_publishes_after_ack_and_advances_sequence() -> anyhow::Result<()> {
    let (_home, startup, session, coordinator) = fixture().await?;
    let live = session.live_thread().unwrap();
    let before = live.experimental_panng_read().await?;
    let starting_sequence = before.commits.last().unwrap().sequence();
    for increment in 1..=2 {
        let mut transaction = coordinator.begin_transaction().await?;
        let stable_id = transaction.commit_id().clone();
        transaction.prepare(
            RuntimeMutation::delta(Vec::new())?,
            /*checkpoint*/ None,
            Vec::new(),
        )?;
        let coordinator_ref = &coordinator;
        let receipt = transaction
            .commit_and_publish(live, |receipt| async move {
                let canonical = live.experimental_panng_read().await?;
                let committed = canonical.commits.last().unwrap();
                assert_eq!(
                    (committed.commit_id(), committed.sequence()),
                    (&stable_id, starting_sequence + increment)
                );
                assert_eq!(canonical.cut.end_byte_offset, receipt.end.end_byte_offset);
                assert!(
                    coordinator_ref.begin_transaction().now_or_never().is_none(),
                    "lease covers publication, not only disk I/O"
                );
                Ok(())
            })
            .await?;
        let canonical = live.experimental_panng_read().await?;
        assert_eq!(canonical.cut.end_byte_offset, receipt.end.end_byte_offset);
    }
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_host_commit_dropped_preparation_fences_without_writing() -> anyhow::Result<()> {
    let (_home, startup, session, coordinator) = fixture().await?;
    let live = session.live_thread().unwrap();
    let before = live.experimental_panng_read().await?.cut;
    let mut transaction = coordinator.begin_transaction().await?;
    transaction.prepare(
        RuntimeMutation::delta(Vec::new())?,
        /*checkpoint*/ None,
        Vec::new(),
    )?;
    drop(transaction);
    assert!(coordinator.begin_transaction().await.is_err());
    assert_eq!(live.experimental_panng_read().await?.cut, before);
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_host_commit_storage_failure_never_publishes() -> anyhow::Result<()> {
    let (_home, startup, session, coordinator) = fixture().await?;
    let live = session.live_thread().unwrap();
    let mut transaction = coordinator.begin_transaction().await?;
    transaction.prepare(
        RuntimeMutation::delta(Vec::new())?,
        /*checkpoint*/ None,
        Vec::new(),
    )?;
    live.shutdown().await?;
    let published = AtomicBool::new(false);
    let result = transaction
        .commit_and_publish(live, |_| async {
            published.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert!(result.is_err());
    assert!(!published.load(Ordering::SeqCst));
    assert!(coordinator.begin_transaction().await.is_err());
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_host_commit_cancelled_publication_fences_after_durable_write() -> anyhow::Result<()>
{
    let (_home, startup, session, coordinator) = fixture().await?;
    let live = session.live_thread().unwrap();
    let before = live.experimental_panng_read().await?.cut;
    let mut transaction = coordinator.begin_transaction().await?;
    let stable_id = transaction.commit_id().clone();
    transaction.prepare(
        RuntimeMutation::delta(Vec::new())?,
        /*checkpoint*/ None,
        Vec::new(),
    )?;
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let mut commit = Box::pin(transaction.commit_and_publish(live, |receipt| async move {
        entered.send(receipt).unwrap();
        std::future::pending::<anyhow::Result<()>>().await
    }));
    let receipt = tokio::select! {
        result = &mut commit => panic!("publication must still be pending: {result:?}"),
        receipt = waiting => receipt?,
    };
    drop(commit);
    assert!(coordinator.begin_transaction().await.is_err());
    let canonical = live.experimental_panng_read().await?;
    assert_eq!(canonical.commits.last().unwrap().commit_id(), &stable_id);
    assert_eq!(canonical.cut.end_byte_offset, receipt.end.end_byte_offset);
    assert!(canonical.cut.end_byte_offset > before.end_byte_offset);
    startup.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn panng_waiting_writer_derives_state_after_prior_ack() -> anyhow::Result<()> {
    let (_home, startup, session, coordinator) = fixture().await?;
    let live = session.live_thread().unwrap();
    let mut first = coordinator.begin_transaction().await?;
    let mut waiting = Box::pin(coordinator.begin_transaction());
    assert!(waiting.as_mut().now_or_never().is_none());
    first.prepare(
        RuntimeMutation::delta(Vec::new())?,
        /*checkpoint*/ None,
        Vec::new(),
    )?;
    let receipt = first.commit_and_publish(live, |_| async { Ok(()) }).await?;
    let mut second = waiting.await?;
    assert_eq!(second.acknowledged()?, receipt);
    second.prepare(
        RuntimeMutation::delta(Vec::new())?,
        /*checkpoint*/ None,
        Vec::new(),
    )?;
    let next = second
        .commit_and_publish(live, |_| async { Ok(()) })
        .await?;
    assert_eq!(
        next.end.end_ordinal_exclusive,
        receipt.end.end_ordinal_exclusive + 1
    );
    startup.cleanup().await;
    Ok(())
}
