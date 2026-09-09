use crate::{
    Cancellation, Error, FaxBackend, FaxMode, OutgoingFax, Receipt, Result, SipProfile,
    SipTransport, TransmissionProgress,
};
use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};

pub struct SipSender {
    service: faxe_native::SipService,
    events: Mutex<Option<mpsc::Receiver<faxe_native::ReceiveEvent>>>,
}
impl SipSender {
    pub fn new() -> Result<Self> {
        let (service, events) = faxe_native::SipService::start()
            .map_err(|error| Error::Transmission(error.to_string()))?;
        Ok(Self {
            service,
            events: Mutex::new(Some(events)),
        })
    }
}
pub(crate) fn account(profile: &SipProfile, password: Option<&str>) -> faxe_native::ServiceAccount {
    faxe_native::ServiceAccount {
        id: profile.id,
        server: profile.server.clone(),
        port: profile.port,
        transport: match profile.transport {
            SipTransport::Udp => faxe_native::SignalingTransport::Udp,
            SipTransport::Tcp => faxe_native::SignalingTransport::Tcp,
            SipTransport::Tls => faxe_native::SignalingTransport::Tls,
        },
        username: profile.username.clone(),
        auth_username: profile.auth_username.clone(),
        password: password.map(str::to_owned),
        register: profile.register,
        outbound_proxy: profile.outbound_proxy.clone(),
        automatic_nat: profile.automatic_nat,
        stun_server: profile.stun_server.clone(),
        audio_playout_delay_ms: profile.audio_playout_delay_ms,
    }
}
pub(crate) fn mode(mode: FaxMode) -> faxe_native::Mode {
    match mode {
        FaxMode::Auto => faxe_native::Mode::Auto,
        FaxMode::T38 => faxe_native::Mode::T38,
        FaxMode::G711 => faxe_native::Mode::G711,
    }
}
impl FaxBackend for SipSender {
    fn receive_service(&self) -> Option<faxe_native::SipService> {
        Some(self.service.clone())
    }
    fn take_receive_events(&self) -> Option<mpsc::Receiver<faxe_native::ReceiveEvent>> {
        self.events.lock().ok()?.take()
    }
    fn send(
        &self,
        fax: OutgoingFax<'_>,
        cancellation: &Cancellation,
        progress: &mut dyn FnMut(TransmissionProgress),
    ) -> Result<Receipt> {
        let cancelled = cancellation.flag();
        let service = self.service.clone();
        cancellation.set_wake(Some(Arc::new(move || service.wake())));
        struct ClearWake<'a>(&'a Cancellation);
        impl Drop for ClearWake<'_> {
            fn drop(&mut self) {
                self.0.set_wake(None);
            }
        }
        let _wake = ClearWake(cancellation);
        let updates = self
            .service
            .submit(
                faxe_native::ServiceSend {
                    account: account(&fax.job.profile, fax.password.map(|p| p.expose())),
                    destination: fax.job.request.destination.clone(),
                    file: fax.tiff_path.to_owned(),
                    station_id: fax.job.profile.station_id.clone(),
                    mode: mode(fax.job.request.mode),
                },
                cancelled.clone(),
            )
            .map_err(|e| Error::Transmission(e.to_string()))?;
        progress(TransmissionProgress::Stage(crate::FaxStage::Connecting));
        loop {
            match updates.recv() {
                Ok(faxe_native::SendUpdate::Progress(event)) => match event {
                    faxe_native::FaxEvent::Stage(stage) => {
                        progress(TransmissionProgress::Stage(stage))
                    }
                    faxe_native::FaxEvent::PageAcknowledged(stats) => {
                        progress(TransmissionProgress::PageAcknowledged(stats.sent_pages))
                    }
                    faxe_native::FaxEvent::T38Activity { transmitted_bytes } => {
                        progress(TransmissionProgress::T38Activity { transmitted_bytes })
                    }
                    faxe_native::FaxEvent::PageProgress(page) => {
                        progress(TransmissionProgress::PageProgress(page))
                    }
                    _ => (),
                },
                Ok(faxe_native::SendUpdate::Finished(result)) => {
                    return match result {
                        Ok(stats) => Ok(Receipt {
                            acknowledged_pages: stats.sent_pages,
                        }),
                        Err(faxe_native::Error::Cancelled) => Err(Error::Cancelled),
                        Err(error) => Err(Error::Transmission(error.to_string())),
                    };
                }
                Err(_) => return Err(Error::WorkerStopped),
            }
        }
    }
}
