use crate::{
    Cancellation, Destination, DocumentInput, Documents, FaxRequest, Job, PreparedDocument, Result,
    SipProfile, Uuid,
};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::broadcast;
/// Secrets are deliberately neither serializable nor printable.
pub struct Password(String);

impl Password {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password([redacted])")
    }
}

pub trait Credentials: Send + Sync + 'static {
    fn password(&self, profile: Uuid) -> Result<Option<Password>>;
    fn save_password(&self, _profile: Uuid, _password: &Password) -> Result<()> {
        Err(crate::Error::Credentials(
            "Credential storage is read-only".into(),
        ))
    }
}

pub struct OutgoingFax<'a> {
    pub job: &'a Job,
    pub tiff_path: &'a Path,
    pub password: Option<&'a Password>,
}

#[derive(Debug, Clone)]
pub enum TransmissionProgress {
    Stage(crate::FaxStage),
    PageAcknowledged(u32),
}

#[derive(Debug, Clone)]
pub struct Receipt {
    pub acknowledged_pages: u32,
}

/// A receipt means T.30 completed successfully, not just that SIP connected.
pub trait FaxBackend: Send + Sync + 'static {
    fn receive_service(&self) -> Option<faxe_native::SipService> {
        None
    }
    fn take_receive_events(
        &self,
    ) -> Option<crossbeam_channel::Receiver<faxe_native::ReceiveEvent>> {
        None
    }
    fn send(
        &self,
        fax: OutgoingFax<'_>,
        cancellation: &Cancellation,
        progress: &mut dyn FnMut(TransmissionProgress),
    ) -> Result<Receipt>;
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    JobChanged(Arc<Job>),
    ReceivedFaxChanged(Arc<crate::ReceivedFax>),
    ReceiverStatus(crate::ReceiverStatus),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub profiles: Vec<SipProfile>,
    pub destinations: Vec<Destination>,
    pub jobs: Vec<Job>,
    pub sending_available: bool,
    pub receive_settings: crate::ReceiveSettings,
    pub receiver_status: crate::ReceiverStatus,
    pub inbox: Vec<crate::ReceivedFax>,
}

/// Blocking compatibility facade for headless callers. The desktop uses EngineHandle.
pub struct Engine(crate::EngineRuntime);
impl Engine {
    pub fn data_directory() -> Result<PathBuf> {
        Ok(crate::AppDirectories::resolve()?.data)
    }
    pub fn open(
        directory: PathBuf,
        credentials: Arc<dyn Credentials>,
        backend: Option<Arc<dyn FaxBackend>>,
    ) -> Result<Self> {
        crate::EngineRuntime::open(directory, credentials, backend).map(Self)
    }
    pub fn documents(&self) -> Documents {
        self.0.handle().documents()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.0.handle().subscribe()
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.0.handle().blocking(|actor| Ok(actor.snapshot()))
    }
    pub fn save_profile(&self, profile: &SipProfile) -> Result<()> {
        let profile = profile.clone();
        self.0.handle().blocking(move |a| a.save_profile(profile))
    }
    pub fn save_destination(&self, destination: &Destination) -> Result<()> {
        let destination = destination.clone();
        self.0
            .handle()
            .blocking(move |a| a.save_destination(destination))
    }
    pub fn configure_receiver(&self, settings: crate::ReceiveSettings) -> Result<()> {
        self.0
            .handle()
            .blocking(move |a| a.configure_receiver(settings))
    }
    pub fn reconnect_receiver(&self) -> Result<()> {
        self.0.handle().blocking(|a| a.reconnect_receiver())
    }
    pub fn cancel_reception(&self, id: Uuid) -> Result<()> {
        self.0.handle().blocking(move |a| a.cancel_reception(id))
    }
    pub fn retry_export(&self, id: Uuid) -> Result<()> {
        self.0.handle().blocking(move |a| a.retry_export(id))
    }
    pub fn clear_queue(&self) -> Result<usize> {
        self.0.handle().blocking(|a| a.clear_queue())
    }
    pub fn enqueue(&self, request: FaxRequest) -> Result<Job> {
        self.0.handle().blocking(move |a| a.enqueue(request))
    }
    pub fn retry(&self, id: Uuid) -> Result<Job> {
        self.0.handle().blocking(move |a| a.retry(id))
    }
    pub fn cancel(&self, id: Uuid) -> Result<()> {
        self.0.handle().blocking(move |a| a.cancel(id))
    }
    pub async fn prepare(
        &self,
        input: DocumentInput,
        cancellation: Cancellation,
    ) -> Result<PreparedDocument> {
        self.0.handle().prepare(input, cancellation).await
    }
    pub fn shutdown(&mut self) -> Result<()> {
        self.0.shutdown_blocking()
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.0.shutdown_blocking();
    }
}
