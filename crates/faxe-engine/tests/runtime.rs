use faxe_engine::*;
use std::{sync::Arc, time::Duration};

struct NoCredentials;
impl Credentials for NoCredentials {
    fn password(&self, _: Uuid) -> Result<Option<Password>> {
        Ok(None)
    }
}
fn profile() -> SipProfile {
    SipProfile {
        id: Uuid::new_v4(),
        name: "Runtime fixture".into(),
        server: "localhost".into(),
        port: 5060,
        transport: SipTransport::Udp,
        username: "fixture".into(),
        auth_username: None,
        register: false,
        outbound_proxy: None,
        audio_playout_delay_ms: 200,
        station_id: "FAXE".into(),
        automatic_nat: false,
        stun_server: None,
        sending_mode: FaxMode::Auto,
    }
}

#[tokio::test]
async fn latest_view_shares_unchanged_collections_and_idle_publishes_nothing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let runtime = EngineRuntime::open(directory.path().into(), Arc::new(NoCredentials), None)?;
    let handle = runtime.handle();
    let mut views = handle.observe();
    let before = views.borrow_and_update().clone();
    let first = profile();
    handle.save_profile(first.clone()).await?;
    let second = profile();
    handle.save_profile(second.clone()).await?;
    views.changed().await.map_err(|_| Error::WorkerStopped)?;
    let after = views.borrow_and_update().clone();
    assert_eq!(after.profiles.len(), 2);
    assert!(after.revision > before.revision);
    assert!(Arc::ptr_eq(&before.jobs, &after.jobs));
    assert!(Arc::ptr_eq(&before.inbox, &after.inbox));
    assert!(
        tokio::time::timeout(Duration::from_millis(150), views.changed())
            .await
            .is_err()
    );
    runtime.shutdown().await?;
    assert_eq!(handle.view().lifecycle, Lifecycle::Stopped);
    assert!(matches!(
        handle.save_profile(first).await,
        Err(Error::WorkerStopped)
    ));
    Ok(())
}

#[tokio::test]
async fn removing_a_destination_updates_observers_and_survives_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let runtime = EngineRuntime::open(directory.path().into(), Arc::new(NoCredentials), None)?;
    let handle = runtime.handle();
    let profile = profile();
    handle.save_profile(profile.clone()).await?;
    let removed = Destination {
        id: Uuid::new_v4(),
        name: "Remove this destination".into(),
        address: "12345".into(),
        profile_id: profile.id,
    };
    // A second bookmark using the same address must remain untouched.
    let kept = Destination {
        id: Uuid::new_v4(),
        name: "Keep this destination".into(),
        ..removed.clone()
    };
    handle.save_destination(removed.clone()).await?;
    handle.save_destination(kept.clone()).await?;
    let mut views = handle.observe();
    let before = views.borrow_and_update().clone();
    handle.remove_destination(removed.id).await?;
    tokio::time::timeout(Duration::from_secs(2), views.changed())
        .await
        .expect("destination removal should publish a view")
        .map_err(|_| Error::WorkerStopped)?;
    let after = views.borrow_and_update().clone();
    assert_eq!(before.destinations.len(), 2);
    assert_eq!(after.destinations.len(), 1);
    assert_eq!(after.destinations[0].id, kept.id);
    assert!(Arc::ptr_eq(&before.profiles, &after.profiles));
    assert!(Arc::ptr_eq(&before.jobs, &after.jobs));
    // Repeated clicks are harmless.
    handle.remove_destination(removed.id).await?;
    runtime.shutdown().await?;

    let reopened = EngineRuntime::open(directory.path().into(), Arc::new(NoCredentials), None)?;
    let view = reopened.handle().view();
    assert_eq!(view.destinations.len(), 1);
    assert_eq!(view.destinations[0].id, kept.id);
    assert_eq!(view.profiles[0].id, profile.id);
    reopened.shutdown().await
}

#[tokio::test]
async fn shutdown_closes_streams_even_when_clients_keep_handles() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut runtime = EngineRuntime::open(directory.path().into(), Arc::new(NoCredentials), None)?;
    let handle = runtime.handle();
    let mut effects = runtime.take_effects().unwrap();
    let mut events = handle.subscribe();
    runtime.shutdown().await?;
    assert!(effects.recv().await.is_none());
    assert!(matches!(
        events.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Closed)
    ));
    // Reopen immediately: shutdown includes store and worker destruction.
    let reopened = EngineRuntime::open(directory.path().into(), Arc::new(NoCredentials), None)?;
    reopened.shutdown().await
}

#[tokio::test]
async fn slow_credentials_do_not_block_commands_and_shutdown_waits_for_owned_work() -> Result<()> {
    struct SlowCredentials {
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
    }
    impl Credentials for SlowCredentials {
        fn password(&self, _: Uuid) -> Result<Option<Password>> {
            Ok(None)
        }
        fn save_password(&self, _: Uuid, _: &Password) -> Result<()> {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            Ok(())
        }
    }
    let directory = tempfile::tempdir()?;
    let (entered, started) = crossbeam_channel::bounded(1);
    let (release, wait) = crossbeam_channel::bounded(1);
    let runtime = EngineRuntime::open(
        directory.path().into(),
        Arc::new(SlowCredentials {
            entered,
            release: wait,
        }),
        None,
    )?;
    let handle = runtime.handle();
    let saving = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .save_profile_with_password(profile(), Some(Password::new("secret".into())))
                .await
        })
    };
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(2)).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), handle.save_profile(profile()))
        .await
        .unwrap()?;
    let mut shutdown = tokio::spawn(runtime.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut shutdown)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    shutdown.await.unwrap()?;
    assert!(matches!(saving.await.unwrap(), Err(Error::WorkerStopped)));
    assert_eq!(handle.view().lifecycle, Lifecycle::Stopped);
    Ok(())
}
