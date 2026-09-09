use faxe_engine::{
    Cancellation, Credentials, DocumentInput, DocumentOptions, Engine, FaxRequest, JobState,
    Password, SipProfile, SipTransport, Uuid,
};
use std::sync::Arc;

struct NoCredentials;

impl Credentials for NoCredentials {
    fn password(&self, _: Uuid) -> faxe_engine::Result<Option<Password>> {
        Ok(None)
    }
}

#[test]
fn reopening_preserves_queue_and_recovers_active_calls_without_redial()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let data = directory.path().join("data");
    let engine = Engine::open(data.clone(), Arc::new(NoCredentials), None)?;
    assert!(Engine::open(data.clone(), Arc::new(NoCredentials), None).is_err());
    let profile = SipProfile {
        id: Uuid::new_v4(),
        name: "Local fixture".into(),
        server: "127.0.0.1".into(),
        port: 5060,
        transport: SipTransport::Udp,
        username: "fixture".into(),
        auth_username: None,
        register: false,
        outbound_proxy: None,
        audio_playout_delay_ms: 200,
        station_id: "FIXTURE".into(),
        automatic_nat: true,
        stun_server: None,
        sending_mode: Default::default(),
    };
    engine.save_profile(&profile)?;
    let source = directory.path().join("page.png");
    image::GrayImage::from_pixel(10, 10, image::Luma([255])).save(&source)?;
    let prepared = engine.documents().prepare(
        DocumentInput {
            paths: vec![source],
            options: DocumentOptions::default(),
        },
        &Cancellation::default(),
        |_| {},
    )?;
    let request = FaxRequest {
        profile_id: profile.id,
        destination: "1234".into(),
        mode: Default::default(),
        document: prepared,
    };
    let queued = engine.enqueue(request.clone())?;
    let mut active = engine.enqueue(request)?;
    active.state = JobState::Sending {
        acknowledged_pages: 0,
    };
    drop(engine);

    // Reproduce the durable state left by a process dying during a call.
    let connection = rusqlite::Connection::open(data.join("faxe.sqlite3"))?;
    connection.execute(
        "UPDATE jobs SET status='active',data=?2 WHERE id=?1",
        rusqlite::params![active.id.to_string(), serde_json::to_string(&active)?],
    )?;
    drop(connection);
    let engine = Engine::open(data, Arc::new(NoCredentials), None)?;
    let snapshot = engine.snapshot()?;
    assert!(matches!(
        snapshot
            .jobs
            .iter()
            .find(|job| job.id == queued.id)
            .unwrap()
            .state,
        JobState::Queued
    ));
    assert!(matches!(
        snapshot
            .jobs
            .iter()
            .find(|job| job.id == active.id)
            .unwrap()
            .state,
        JobState::Interrupted
    ));
    let retry = engine.retry(active.id)?;
    assert_ne!(retry.id, active.id);
    assert_eq!(retry.retry_of, Some(active.id));
    engine.cancel(queued.id)?;
    assert!(engine.retry(retry.id).is_err());
    assert_eq!(engine.clear_queue()?, 3);
    assert!(engine.snapshot()?.jobs.is_empty());
    assert_eq!(engine.snapshot()?.profiles.len(), 1);
    assert!(
        engine
            .documents()
            .fax_path(queued.request.document.id)
            .exists()
    );
    Ok(())
}

#[test]
fn clearing_an_active_queue_does_not_resurrect_jobs_or_delete_documents()
-> Result<(), Box<dyn std::error::Error>> {
    use faxe_engine::{FaxBackend, FaxStage, OutgoingFax, Receipt, TransmissionProgress};
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    struct ActiveSend(mpsc::Sender<()>);
    impl FaxBackend for ActiveSend {
        fn send(
            &self,
            _: OutgoingFax<'_>,
            cancellation: &Cancellation,
            progress: &mut dyn FnMut(TransmissionProgress),
        ) -> faxe_engine::Result<Receipt> {
            progress(TransmissionProgress::Stage(FaxStage::Calling));
            progress(TransmissionProgress::Stage(FaxStage::Ringing));
            self.0.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match cancellation.is_cancelled() {
                    true => {
                        progress(TransmissionProgress::Stage(FaxStage::Connected));
                        return Err(faxe_engine::Error::Cancelled);
                    }
                    false if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    false => panic!("active fax was not cancelled"),
                }
            }
        }
    }

    let directory = tempfile::tempdir()?;
    let (started, events) = mpsc::channel();
    let engine = Engine::open(
        directory.path().join("data"),
        Arc::new(NoCredentials),
        Some(Arc::new(ActiveSend(started))),
    )?;
    let profile = SipProfile {
        id: Uuid::new_v4(),
        name: "Queue fixture".into(),
        server: "127.0.0.1".into(),
        port: 5060,
        transport: SipTransport::Udp,
        username: "fixture".into(),
        auth_username: None,
        register: false,
        outbound_proxy: None,
        audio_playout_delay_ms: 200,
        station_id: "FIXTURE".into(),
        automatic_nat: true,
        stun_server: None,
        sending_mode: Default::default(),
    };
    engine.save_profile(&profile)?;
    let source = directory.path().join("page.png");
    image::GrayImage::from_pixel(10, 10, image::Luma([255])).save(&source)?;
    let document = engine.documents().prepare(
        DocumentInput {
            paths: vec![source],
            options: DocumentOptions::default(),
        },
        &Cancellation::default(),
        |_| {},
    )?;
    let request = FaxRequest {
        profile_id: profile.id,
        destination: "1234".into(),
        mode: Default::default(),
        document,
    };
    engine.enqueue(request.clone())?;
    events.recv_timeout(Duration::from_secs(5))?;
    let active = &engine.snapshot()?.jobs[0];
    assert_eq!(
        active
            .steps
            .iter()
            .map(|step| step.stage)
            .collect::<Vec<_>>(),
        vec![FaxStage::Calling, FaxStage::Ringing]
    );
    engine.enqueue(request.clone())?;
    assert_eq!(engine.clear_queue()?, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !engine.snapshot()?.jobs.is_empty() {
        assert!(Instant::now() < deadline, "cancelled job was resurrected");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(engine.snapshot()?.profiles.len(), 1);
    assert!(engine.documents().fax_path(request.document.id).exists());

    let next = engine.enqueue(request)?;
    events.recv_timeout(Duration::from_secs(5))?;
    engine.cancel(next.id)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = engine.snapshot()?;
        assert_eq!(snapshot.jobs.len(), 1);
        match snapshot.jobs[0].state {
            JobState::Cancelled { .. } => break,
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => panic!("subsequent cancellation was not persisted"),
        }
    }
    Ok(())
}
