use faxe_engine::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct NoCredentials;
impl Credentials for NoCredentials {
    fn password(&self, _: Uuid) -> Result<Option<Password>> {
        Ok(None)
    }
}
struct GatedBackend {
    started: crossbeam_channel::Sender<Uuid>,
    finish: AtomicBool,
}
impl FaxBackend for GatedBackend {
    fn send(
        &self,
        fax: OutgoingFax<'_>,
        cancel: &Cancellation,
        _: &mut dyn FnMut(TransmissionProgress),
    ) -> Result<Receipt> {
        self.started.send(fax.job.id).unwrap();
        while !self.finish.load(Ordering::Acquire) {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(Receipt {
            acknowledged_pages: fax.job.request.document.pages,
        })
    }
}
fn profile(port: u16) -> SipProfile {
    SipProfile {
        id: Uuid::new_v4(),
        name: "Fixture".into(),
        server: "127.0.0.1".into(),
        port,
        transport: SipTransport::Udp,
        username: "sender".into(),
        auth_username: None,
        register: false,
        outbound_proxy: None,
        station_id: "FAXE".into(),
        sending_mode: FaxMode::T38,
        automatic_nat: false,
        stun_server: None,
        audio_playout_delay_ms: 200,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_preparation_sends_and_isolated_cancel() -> Result<()> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("page.png");
    image::GrayImage::from_pixel(16, 16, image::Luma([255])).save(&source)?;
    let (started, starts) = crossbeam_channel::unbounded();
    let backend = Arc::new(GatedBackend {
        started,
        finish: AtomicBool::new(false),
    });
    let options = EngineOptions {
        limits: CallLimits {
            outgoing: 2,
            incoming: 2,
        },
        document_workers: 2,
        ..Default::default()
    };
    let runtime = EngineRuntime::open_with_options(
        root.path().join("data"),
        Arc::new(NoCredentials),
        Some(backend.clone()),
        options,
    )?;
    let handle = runtime.handle();
    let profile = profile(5060);
    handle.save_profile(profile.clone()).await?;
    let prepare = || {
        handle.prepare(
            DocumentInput {
                paths: vec![source.clone()],
                options: DocumentOptions::default(),
            },
            Cancellation::default(),
        )
    };
    let (a, b) = tokio::join!(prepare(), prepare());
    let (a, b) = (a?, b?);
    assert_ne!(a.id, b.id);
    let request = |document| FaxRequest {
        profile_id: profile.id,
        destination: "123".into(),
        mode: FaxMode::T38,
        document,
    };
    let first = handle.enqueue(request(a.clone())).await?;
    let second = handle.enqueue(request(b)).await?;
    let third = handle.enqueue(request(a)).await?;
    let _starts = tokio::task::spawn_blocking(move || {
        let mut ids = vec![
            starts.recv_timeout(Duration::from_secs(3)).unwrap(),
            starts.recv_timeout(Duration::from_secs(3)).unwrap(),
        ];
        ids.sort();
        let mut expected = vec![first.id, second.id];
        expected.sort();
        assert_eq!(ids, expected);
        starts
    })
    .await
    .unwrap();
    assert!(matches!(
        handle.job(third.id).await?.state,
        JobState::Queued
    ));
    handle.cancel(first.id).await?;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), handle.wait_job(first.id))
            .await
            .unwrap()?
            .state,
        JobState::Cancelled { .. }
    ));
    assert!(handle.job(second.id).await?.state.is_active());
    backend.finish.store(true, Ordering::Release);
    for id in [second.id, third.id] {
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), handle.wait_job(id))
                .await
                .unwrap()?
                .state,
            JobState::Succeeded { .. }
        ));
    }
    runtime.shutdown().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_loopback_faxes_have_independent_media_and_output() -> Result<()> {
    let root = tempfile::tempdir()?;
    let output = root.path().join("received");
    std::fs::create_dir(&output)?;
    let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let address = socket.local_addr()?;
    drop(socket);
    let options = EngineOptions {
        limits: CallLimits {
            outgoing: 2,
            incoming: 2,
        },
        document_workers: 2,
        ..Default::default()
    };
    let backend = Arc::new(SipSender::with_options(&options)?);
    let mut runtime = EngineRuntime::open_with_options(
        root.path().join("data"),
        Arc::new(NoCredentials),
        Some(backend),
        options,
    )?;
    let handle = runtime.handle();
    let mut offers = runtime.take_incoming().unwrap();
    let profile = profile(address.port());
    handle.save_profile(profile.clone()).await?;
    handle
        .configure_receiver(ReceiveSettings {
            listener: Some(ListenerConfig {
                bind: address,
                transport: SignalingTransport::Udp,
                advertised_ip: None,
            }),
            folder: Some(output),
            mode: FaxMode::T38,
            manual: true,
            ..Default::default()
        })
        .await?;
    let mut views = handle.observe();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                views.borrow_and_update().receiver_status.state,
                ReceiverState::Ready
            ) {
                break;
            }
            views.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let path = root.path().join("page.png");
    image::GrayImage::from_pixel(16, 16, image::Luma([255])).save(&path)?;
    let doc = handle
        .prepare(
            DocumentInput {
                paths: vec![path],
                options: DocumentOptions::default(),
            },
            Cancellation::default(),
        )
        .await?;
    let mut jobs = Vec::new();
    for destination in ["alice", "bob"] {
        jobs.push(
            handle
                .enqueue(FaxRequest {
                    profile_id: profile.id,
                    destination: format!("sip:{destination}@{address}"),
                    mode: FaxMode::T38,
                    document: doc.clone(),
                })
                .await?,
        );
    }
    let mut receptions = Vec::new();
    for _ in 0..2 {
        let offer = tokio::time::timeout(Duration::from_secs(4), offers.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(offer.destination.contains("alice") || offer.destination.contains("bob"));
        receptions.push(handle.accept_incoming(offer.id).await?);
    }
    assert_ne!(receptions[0].id, receptions[1].id);
    assert_ne!(receptions[0].tiff_path, receptions[1].tiff_path);
    for job in jobs {
        let result = tokio::time::timeout(Duration::from_secs(90), handle.wait_job(job.id))
            .await
            .unwrap()?;
        assert!(
            matches!(
                result.state,
                JobState::Succeeded {
                    acknowledged_pages: 1
                }
            ),
            "{:?}",
            result.state
        );
    }
    let mut paths = Vec::new();
    for fax in receptions {
        let result = tokio::time::timeout(Duration::from_secs(10), handle.wait_reception(fax.id))
            .await
            .unwrap()?;
        assert!(result.profile_id.is_none());
        assert_eq!(result.outcome, ReceptionOutcome::Received);
        if let ExportStatus::Published { path } = result.export {
            assert!(path.is_file());
            paths.push(path);
        } else {
            panic!("{:?}", result.export);
        }
    }
    assert_ne!(paths[0], paths[1]);
    runtime.shutdown().await
}
