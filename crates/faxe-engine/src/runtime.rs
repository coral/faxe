//! Application state and persistence have one owner. Handles only exchange values.
use crate::*;
use crossbeam_channel as channel;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

type Request = Box<dyn FnOnce(&mut Actor) + Send>;
enum Internal {
    Finished(Request),
    Message(Request),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Starting,
    Running,
    Stopping,
    Stopped,
}

/// A cached read model. Unchanged collections are shared across revisions.
#[derive(Debug, Clone)]
pub struct EngineView {
    pub revision: u64,
    pub lifecycle: Lifecycle,
    pub profiles: Arc<Vec<SipProfile>>,
    pub destinations: Arc<Vec<Destination>>,
    pub jobs: Arc<Vec<Job>>,
    pub sending_available: bool,
    pub receive_settings: ReceiveSettings,
    pub receiver_status: ReceiverStatus,
    pub inbox: Arc<Vec<ReceivedFax>>,
    pub error: Option<String>,
}
impl Default for EngineView {
    fn default() -> Self {
        Self {
            revision: 0,
            lifecycle: Lifecycle::Starting,
            profiles: Default::default(),
            destinations: Default::default(),
            jobs: Default::default(),
            sending_available: false,
            receive_settings: Default::default(),
            receiver_status: Default::default(),
            inbox: Default::default(),
            error: None,
        }
    }
}
#[derive(Debug, Clone)]
pub enum EngineEffect {
    Error(String),
    ReceptionFinished {
        id: Uuid,
        caller: String,
        pages: u32,
    },
    ReceivedPdfReady {
        id: Uuid,
        path: PathBuf,
    },
}
pub struct Preview {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Owns the worker lifetime. Client handles never own or join workers.
pub struct EngineRuntime {
    handle: EngineHandle,
    stop: channel::Sender<()>,
    worker: Option<thread::JoinHandle<Result<()>>>,
    effects: Option<mpsc::UnboundedReceiver<EngineEffect>>,
    incoming: Option<mpsc::Receiver<IncomingFax>>,
}
#[derive(Clone)]
pub struct EngineHandle {
    requests: channel::Sender<Request>,
    view: watch::Receiver<Arc<EngineView>>,
    events: Arc<broadcast::Receiver<EngineEvent>>,
    documents: Documents,
    stopped: Arc<AtomicBool>,
    document_slots: Arc<tokio::sync::Semaphore>,
}
impl EngineRuntime {
    /// Blocking startup; call from a blocking worker, never the window thread.
    pub fn open(
        directory: PathBuf,
        credentials: Arc<dyn Credentials>,
        backend: Option<Arc<dyn FaxBackend>>,
    ) -> Result<Self> {
        Self::open_with_options(directory, credentials, backend, EngineOptions::default())
    }
    pub fn open_with_options(
        directory: PathBuf,
        credentials: Arc<dyn Credentials>,
        backend: Option<Arc<dyn FaxBackend>>,
        options: EngineOptions,
    ) -> Result<Self> {
        options.validate()?;
        let document_slots = Arc::new(tokio::sync::Semaphore::new(options.document_workers));
        let (incoming, incoming_receiver) = mpsc::channel(options.limits.incoming);
        let (requests, inbox) = channel::bounded::<Request>(64);
        let (completion, completed) = channel::unbounded::<Internal>();
        let (stop, stopping) = channel::bounded(1);
        let (views, view) = watch::channel(Arc::new(EngineView::default()));
        let (events, event_receiver) = broadcast::channel(128);
        let (effects, effect_receiver) = mpsc::unbounded_channel();
        let (ready, opened) = channel::bounded(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_events = events.clone();
        let worker_stopped = stopped.clone();
        let worker = thread::Builder::new()
            .name("faxe-engine".into())
            .spawn(move || {
                let start = Actor::open(
                    directory,
                    credentials,
                    backend,
                    options,
                    ActorChannels {
                        completion,
                        views,
                        events: worker_events,
                        effects,
                        incoming,
                    },
                );
                match start {
                    Ok(mut actor) => {
                        actor.publish();
                        let _ = ready.send(Ok(actor.documents.clone()));
                        let result = actor.run(inbox, completed, stopping);
                        worker_stopped.store(true, Ordering::Release);
                        result
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        worker_stopped.store(true, Ordering::Release);
                        Ok(())
                    }
                }
            })?;
        match opened.recv() {
            Ok(Ok(documents)) => Ok(Self {
                handle: EngineHandle {
                    requests,
                    view,
                    events: Arc::new(event_receiver),
                    documents,
                    stopped,
                    document_slots,
                },
                stop,
                worker: Some(worker),
                effects: Some(effect_receiver),
                incoming: Some(incoming_receiver),
            }),
            result => {
                let _ = stop.try_send(());
                let _ = worker.join();
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => Error::WorkerStopped,
                })
            }
        }
    }
    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }
    pub fn take_effects(&mut self) -> Option<mpsc::UnboundedReceiver<EngineEffect>> {
        self.effects.take()
    }
    pub fn take_incoming(&mut self) -> Option<mpsc::Receiver<IncomingFax>> {
        self.incoming.take()
    }
    pub fn request_shutdown(&self) {
        self.handle.document_slots.close();
        self.handle.stopped.store(true, Ordering::Release);
        let _ = self.stop.try_send(());
    }
    pub fn shutdown_blocking(&mut self) -> Result<()> {
        self.request_shutdown();
        match self.worker.take() {
            Some(worker) => worker.join().map_err(|_| Error::WorkerStopped)?,
            None => Ok(()),
        }
    }
    pub async fn shutdown(mut self) -> Result<()> {
        self.request_shutdown();
        tokio::task::spawn_blocking(move || self.shutdown_blocking())
            .await
            .map_err(|_| Error::WorkerStopped)?
    }
}
impl Drop for EngineRuntime {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}
impl EngineHandle {
    pub async fn job(&self, id: Uuid) -> Result<Job> {
        self.request(move |a| a.store.job(id)).await
    }
    pub async fn reception(&self, id: Uuid) -> Result<ReceivedFax> {
        self.request(move |a| a.store.received_fax(id)).await
    }
    pub async fn accept_incoming(&self, id: Uuid) -> Result<ReceivedFax> {
        self.request(move |a| a.accept_incoming(id)).await
    }
    pub async fn reject_incoming(&self, id: Uuid) -> Result<()> {
        self.request(move |a| a.reject_incoming(id)).await
    }
    pub async fn wait_job(&self, id: Uuid) -> Result<Job> {
        let mut view = self.observe();
        loop {
            view.borrow_and_update();
            let job = self.job(id).await?;
            if job.state.is_finished() {
                return Ok(job);
            }
            view.changed().await.map_err(|_| Error::WorkerStopped)?;
        }
    }
    pub async fn wait_reception(&self, id: Uuid) -> Result<ReceivedFax> {
        let mut view = self.observe();
        loop {
            view.borrow_and_update();
            let fax = self.reception(id).await?;
            if fax.is_finished() {
                return Ok(fax);
            }
            view.changed().await.map_err(|_| Error::WorkerStopped)?;
        }
    }
    pub async fn save_settings(&self, settings: Settings) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.request(move |actor| actor.queue_settings(settings, reply))
            .await?;
        result.await.map_err(|_| Error::WorkerStopped)?
    }
    pub async fn preview(&self, id: Uuid, page: u32) -> Result<Preview> {
        let (send, recv) = oneshot::channel();
        self.request(move |a| {
            let path = a.documents.preview_path(id, page);
            a.spawn("faxe-preview", move || {
                let result = image::ImageReader::open(path)
                    .map_err(Error::from)
                    .and_then(|reader| {
                        let image = reader.decode()?.into_rgba8();
                        Ok(Preview {
                            width: image.width(),
                            height: image.height(),
                            pixels: image.into_raw(),
                        })
                    });
                Box::new(move |_| {
                    let _ = send.send(result);
                })
            })
        })
        .await?;
        recv.await.map_err(|_| Error::WorkerStopped)?
    }
    pub fn documents(&self) -> Documents {
        self.documents.clone()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.resubscribe()
    }
    pub fn observe(&self) -> watch::Receiver<Arc<EngineView>> {
        self.view.clone()
    }
    pub fn view(&self) -> Arc<EngineView> {
        self.view.borrow().clone()
    }
    pub(crate) fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Actor) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (send, recv) = channel::bounded(1);
        self.submit(Box::new(move |a| {
            let result = f(a);
            a.publish();
            let _ = send.send(result);
        }))?;
        recv.recv().map_err(|_| Error::WorkerStopped)?
    }
    fn submit(&self, request: Request) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Error::WorkerStopped);
        }
        self.requests
            .send(request)
            .map_err(|_| Error::WorkerStopped)
    }
    async fn request<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Actor) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (send, recv) = oneshot::channel();
        let handle = self.clone();
        tokio::task::spawn_blocking(move || {
            handle.submit(Box::new(move |a| {
                if a.stopping {
                    let _ = send.send(Err(Error::WorkerStopped));
                    return;
                }
                let result = f(a);
                a.publish();
                let _ = send.send(result);
            }))
        })
        .await
        .map_err(|_| Error::WorkerStopped)??;
        recv.await.map_err(|_| Error::WorkerStopped)?
    }
    pub async fn save_profile_with_password(
        &self,
        profile: SipProfile,
        password: Option<Password>,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.request(move |actor| {
            profile.validate()?;
            let credentials = actor.credentials.clone();
            actor.spawn("faxe-profile", move || {
                let saved = match password {
                    Some(password) => credentials.save_password(profile.id, &password),
                    None => Ok(()),
                };
                Box::new(move |actor| {
                    let result = saved.and_then(|_| {
                        if actor.stopping {
                            Err(Error::WorkerStopped)
                        } else {
                            actor.save_profile(profile)
                        }
                    });
                    let _ = reply.send(result);
                })
            })
        })
        .await?;
        result.await.map_err(|_| Error::WorkerStopped)?
    }
    pub async fn save_profile(&self, profile: SipProfile) -> Result<()> {
        self.request(move |a| a.save_profile(profile)).await
    }
    pub async fn save_destination(&self, destination: Destination) -> Result<()> {
        self.request(move |a| a.save_destination(destination)).await
    }
    pub async fn remove_destination(&self, id: Uuid) -> Result<()> {
        self.request(move |a| a.remove_destination(id)).await
    }
    pub async fn configure_receiver(&self, settings: ReceiveSettings) -> Result<()> {
        self.request(move |a| a.configure_receiver(settings)).await
    }
    pub async fn reconnect_receiver(&self) -> Result<()> {
        self.request(|a| a.reconnect_receiver()).await
    }
    pub async fn cancel_reception(&self, id: Uuid) -> Result<()> {
        self.request(move |a| a.cancel_reception(id)).await
    }
    pub async fn retry_export(&self, id: Uuid) -> Result<()> {
        self.request(move |a| a.retry_export(id)).await
    }
    pub async fn clear_queue(&self) -> Result<usize> {
        self.request(|a| a.clear_queue()).await
    }
    pub async fn enqueue(&self, request: FaxRequest) -> Result<Job> {
        self.request(move |a| a.enqueue(request)).await
    }
    pub async fn retry(&self, id: Uuid) -> Result<Job> {
        self.request(move |a| a.retry(id)).await
    }
    pub async fn cancel(&self, id: Uuid) -> Result<()> {
        self.request(move |a| a.cancel(id)).await
    }
    pub async fn prepare(
        &self,
        input: DocumentInput,
        cancellation: Cancellation,
    ) -> Result<PreparedDocument> {
        self.prepare_document(cancellation, move |documents, cancellation| {
            tracing::info!(sources = input.paths.len(), "Preparing document");
            documents.prepare(input, cancellation, |_| {})
        })
        .await
    }

    pub async fn adjust_document(
        &self,
        id: Uuid,
        options: crate::DocumentOptions,
        cancellation: Cancellation,
    ) -> Result<PreparedDocument> {
        self.prepare_document(cancellation, move |documents, cancellation| {
            documents.adjust(id, options, cancellation, |_| {})
        })
        .await
    }

    async fn prepare_document(
        &self,
        cancellation: Cancellation,
        prepare: impl FnOnce(Documents, &Cancellation) -> Result<PreparedDocument> + Send + 'static,
    ) -> Result<PreparedDocument> {
        let permit = tokio::select! {
            permit = self.document_slots.clone().acquire_owned() => permit.map_err(|_| Error::WorkerStopped)?,
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
        };
        cancellation.check()?;
        let (send, recv) = oneshot::channel();
        let id = Uuid::new_v4();
        self.request(move |a| {
            a.preparing.insert(id, cancellation.clone());
            let documents = a.documents.clone();
            let result = a.spawn("faxe-document", move || {
                let _permit = permit;
                let result = prepare(documents, &cancellation);
                Box::new(move |a| {
                    a.preparing.remove(&id);
                    let _ = send.send(result);
                })
            });
            if result.is_err() {
                a.preparing.remove(&id);
            }
            result
        })
        .await?;
        recv.await.map_err(|_| Error::WorkerStopped)?
    }
}

type Work = Box<dyn FnOnce() -> Request + Send>;
struct BlockingPool {
    sender: Option<channel::Sender<Work>>,
    workers: Vec<thread::JoinHandle<()>>,
}
impl BlockingPool {
    fn new(completion: channel::Sender<Internal>) -> Result<Self> {
        let (sender, receiver) = channel::bounded::<Work>(64);
        let mut pool = Self {
            sender: Some(sender),
            workers: Vec::new(),
        };
        for index in 0..2 {
            let receiver = receiver.clone();
            let completion = completion.clone();
            pool.workers.push(
                thread::Builder::new()
                    .name(format!("faxe-io-{index}"))
                    .spawn(move || {
                        for work in receiver {
                            let request = complete(work);
                            let _ = completion.send(Internal::Finished(request));
                        }
                    })?,
            );
        }
        Ok(pool)
    }
}
impl Drop for BlockingPool {
    fn drop(&mut self) {
        self.sender.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
fn complete(work: impl FnOnce() -> Request) -> Request {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|_| {
        Box::new(|a: &mut Actor| {
            a.error(Error::WorkerStopped);
            a.stop();
        })
    })
}

struct PendingIncoming {
    request: faxe_native::AdmissionRequest,
    profile_id: Option<Uuid>,
    profile_name: String,
    options: ReceiveSettings,
}

struct ActorChannels {
    completion: channel::Sender<Internal>,
    views: watch::Sender<Arc<EngineView>>,
    events: broadcast::Sender<EngineEvent>,
    effects: mpsc::UnboundedSender<EngineEffect>,
    incoming: mpsc::Sender<IncomingFax>,
}

pub(crate) struct Actor {
    store: Store,
    directory: PathBuf,
    documents: Documents,
    credentials: Arc<dyn Credentials>,
    backend: Option<Arc<dyn FaxBackend>>,
    service: Option<faxe_native::SipService>,
    native: channel::Receiver<faxe_native::ReceiveEvent>,
    completion: channel::Sender<Internal>,
    workers: Vec<thread::JoinHandle<()>>,
    pool: BlockingPool,
    configuring: bool,
    settings_saving: bool,
    settings_pending: Option<(Settings, Vec<oneshot::Sender<Result<()>>>)>,
    working: usize,
    active: HashMap<Uuid, (Job, Cancellation)>,
    options: EngineOptions,
    incoming: mpsc::Sender<IncomingFax>,
    admissions: HashMap<Uuid, PendingIncoming>,
    preparing: HashMap<Uuid, Cancellation>,
    exports: VecDeque<Uuid>,
    exporting: Option<Uuid>,
    generation: u64,
    stopping: bool,
    clear_active: HashSet<Uuid>,
    dirty: bool,
    view: EngineView,
    views: watch::Sender<Arc<EngineView>>,
    events: broadcast::Sender<EngineEvent>,
    effects: mpsc::UnboundedSender<EngineEffect>,
}
impl Actor {
    fn open(
        directory: PathBuf,
        credentials: Arc<dyn Credentials>,
        backend: Option<Arc<dyn FaxBackend>>,
        options: EngineOptions,
        channels: ActorChannels,
    ) -> Result<Self> {
        let ActorChannels {
            completion,
            views,
            events,
            effects,
            incoming,
        } = channels;
        let store = Store::open(&directory).map_err(|error| {
            Error::Invalid(format!(
                "Opening fax database at {}: {error}",
                directory.display()
            ))
        })?;
        let mut documents = Documents::new(directory.join("spool")).map_err(|error| {
            Error::Invalid(format!(
                "Opening document spool at {}: {error}",
                directory.display()
            ))
        })?;
        if let Ok(dirs) = AppDirectories::resolve()
            && directory == dirs.data
        {
            documents = documents.with_cache(dirs.cache);
        }
        let service = backend.as_ref().and_then(|b| b.receive_service());
        let native = backend
            .as_ref()
            .and_then(|b| b.take_receive_events())
            .unwrap_or_else(channel::never);
        let view = EngineView {
            profiles: Arc::new(store.profiles()?),
            destinations: Arc::new(store.destinations()?),
            jobs: Arc::new(store.jobs()?),
            inbox: Arc::new(store.received_faxes()?),
            receive_settings: store.receive_settings()?,
            sending_available: backend.is_some(),
            lifecycle: Lifecycle::Running,
            ..Default::default()
        };
        let exports = view
            .inbox
            .iter()
            .filter(|fax| {
                !fax.outcome.is_active()
                    && matches!(
                        fax.export,
                        ExportStatus::Pending
                            | ExportStatus::Publishing { .. }
                            | ExportStatus::Failed { .. }
                    )
            })
            .map(|fax| fax.id)
            .collect();
        let pool = BlockingPool::new(completion.clone())?;
        Ok(Self {
            store,
            directory,
            documents,
            credentials,
            backend,
            service,
            native,
            pool,
            configuring: false,
            settings_saving: false,
            settings_pending: None,
            completion,
            workers: vec![],
            working: 0,
            active: HashMap::new(),
            options,
            incoming,
            admissions: HashMap::new(),
            preparing: HashMap::new(),
            exports,
            exporting: None,
            generation: 0,
            stopping: false,
            clear_active: HashSet::new(),
            dirty: true,
            view,
            views,
            events,
            effects,
        })
    }
    fn run(
        &mut self,
        mut requests: channel::Receiver<Request>,
        completed: channel::Receiver<Internal>,
        mut stop: channel::Receiver<()>,
    ) -> Result<()> {
        if let Err(error) = self.refresh_receiver() {
            self.error(error);
        }
        self.publish();
        loop {
            if !self.stopping {
                while self.active.len() < self.options.limits.outgoing {
                    let before = self.active.len();
                    if let Err(error) = self.start_send() {
                        self.error(error);
                        break;
                    }
                    if self.active.len() == before {
                        break;
                    }
                }
                if let Err(error) = self.start_export() {
                    self.error(error);
                }
            }
            self.publish();
            if self.stopping && self.working == 0 {
                for event in self.native.try_iter().collect::<Vec<_>>() {
                    if let Err(error) = self.received(event) {
                        self.error(error);
                    }
                }
                break;
            }
            channel::select! {
                recv(stop) -> _ => { self.stop(); stop=channel::never(); requests=channel::never(); }
                recv(requests) -> request => match request { Ok(request) => request(self), Err(_) => { self.stop(); requests=channel::never(); } },
                recv(completed) -> request => if let Ok(message)=request { match message { Internal::Finished(request) => { self.working-=1; request(self); }, Internal::Message(request) => request(self) } },
                recv(self.native) -> event => match event { Ok(event) => if let Err(error)=self.received(event) { self.error(error); }, Err(_) => { self.native=channel::never(); if !self.stopping { self.error(Error::WorkerStopped); } } },
            }
            let mut i = 0;
            while i < self.workers.len() {
                if self.workers[i].is_finished() {
                    let _ = self.workers.swap_remove(i).join();
                } else {
                    i += 1;
                }
            }
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        self.view.lifecycle = Lifecycle::Stopped;
        self.dirty = true;
        self.publish();
        Ok(())
    }
    fn spawn(&mut self, name: &str, work: impl FnOnce() -> Request + Send + 'static) -> Result<()> {
        if matches!(
            name,
            "faxe-credentials" | "faxe-preview" | "faxe-profile" | "faxe-settings"
        ) {
            self.pool
                .sender
                .as_ref()
                .ok_or(Error::WorkerStopped)?
                .try_send(Box::new(work))
                .map_err(|_| Error::Invalid("Background work queue is full".into()))?;
            self.working += 1;
            return Ok(());
        }
        let send = self.completion.clone();
        let worker = thread::Builder::new().name(name.into()).spawn(move || {
            let completed = complete(work);
            let _ = send.send(Internal::Finished(completed));
        })?;
        self.workers.push(worker);
        self.working += 1;
        Ok(())
    }
    fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.generation += 1;
        self.view.lifecycle = Lifecycle::Stopping;
        self.dirty = true;
        for (_, cancellation) in self.active.values() {
            cancellation.cancel();
        }
        for cancellation in self.preparing.values() {
            cancellation.cancel();
        }
        for (_, pending) in self.admissions.drain() {
            let _ = pending.request.reject();
        }
        if let Some(service) = self.service.take()
            && let Err(error) = self.spawn("faxe-stop", move || {
                service.shutdown();
                Box::new(|_| {})
            })
        {
            self.error(error);
        }
    }
    fn publish(&mut self) {
        if self.dirty {
            self.view.revision += 1;
            self.views.send_replace(Arc::new(self.view.clone()));
            self.dirty = false;
        }
    }
    fn emit(&mut self, event: EngineEvent) {
        let _ = self.events.send(event);
        self.dirty = true;
    }
    fn error(&mut self, error: Error) {
        let message = error.to_string();
        self.view.error = Some(message.clone());
        let _ = self.effects.send(EngineEffect::Error(message.clone()));
        self.emit(EngineEvent::Error(message));
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            profiles: (*self.view.profiles).clone(),
            destinations: (*self.view.destinations).clone(),
            jobs: (*self.view.jobs).clone(),
            sending_available: self.view.sending_available,
            receive_settings: self.view.receive_settings.clone(),
            receiver_status: self.view.receiver_status.clone(),
            inbox: (*self.view.inbox).clone(),
        }
    }
    fn job_changed(&mut self, job: Job) {
        if self.view.jobs.iter().any(|old| old == &job) {
            return;
        }
        upsert(Arc::make_mut(&mut self.view.jobs), job.clone(), |j| j.id);
        self.emit(EngineEvent::JobChanged(Arc::new(job)));
    }
    fn received_changed(&mut self, fax: ReceivedFax) {
        if self.view.inbox.iter().any(|old| old == &fax) {
            return;
        }
        if let ExportStatus::Published { path } = &fax.export
            && self.view.inbox.iter().any(|old| {
                old.id == fax.id && !matches!(old.export, ExportStatus::Published { .. })
            })
        {
            let _ = self.effects.send(EngineEffect::ReceivedPdfReady {
                id: fax.id,
                path: path.clone(),
            });
        }
        let terminal = |fax: &ReceivedFax| {
            matches!(
                fax.export,
                ExportStatus::Published { .. }
                    | ExportStatus::Failed { .. }
                    | ExportStatus::NoContent
            )
        };
        if fax.options.notifications
            && terminal(&fax)
            && self
                .view
                .inbox
                .iter()
                .any(|old| old.id == fax.id && !terminal(old))
        {
            let _ = self.effects.send(EngineEffect::ReceptionFinished {
                id: fax.id,
                caller: fax.caller.clone(),
                pages: fax.recovered_pages,
            });
        }
        upsert(Arc::make_mut(&mut self.view.inbox), fax.clone(), |f| f.id);
        self.emit(EngineEvent::ReceivedFaxChanged(Arc::new(fax)));
    }
    pub(crate) fn save_profile(&mut self, profile: SipProfile) -> Result<()> {
        self.store.save_profile(&profile)?;
        let receiving = self.view.receive_settings.profile == Some(profile.id);
        upsert(Arc::make_mut(&mut self.view.profiles), profile, |p| p.id);
        Arc::make_mut(&mut self.view.profiles).sort_by(|a, b| a.name.cmp(&b.name));
        self.dirty = true;
        if receiving {
            self.refresh_receiver()?;
        }
        Ok(())
    }
    pub(crate) fn save_destination(&mut self, destination: Destination) -> Result<()> {
        self.store.save_destination(&destination)?;
        upsert(
            Arc::make_mut(&mut self.view.destinations),
            destination,
            |d| d.id,
        );
        Arc::make_mut(&mut self.view.destinations).sort_by(|a, b| a.name.cmp(&b.name));
        self.dirty = true;
        Ok(())
    }
    pub(crate) fn remove_destination(&mut self, id: Uuid) -> Result<()> {
        self.store.remove_destination(id)?;
        Arc::make_mut(&mut self.view.destinations).retain(|destination| destination.id != id);
        self.dirty = true;
        Ok(())
    }
    pub(crate) fn configure_receiver(&mut self, mut settings: ReceiveSettings) -> Result<()> {
        let previous = &self.view.receive_settings;
        if settings.folder.is_none() {
            settings.folder = previous.folder.clone();
        }
        settings.validate(&self.view.profiles)?;
        if (settings.profile.is_some() || settings.listener.is_some())
            && (settings.profile != previous.profile
                || settings.listener != previous.listener
                || settings.folder != previous.folder)
            && settings.folder.as_ref().is_none_or(|f| !f.is_dir())
        {
            return Err(Error::Invalid("Receive folder is unavailable".into()));
        }
        self.store.save_receive_settings(&settings)?;
        self.view.receive_settings = settings;
        self.dirty = true;
        self.refresh_receiver()
    }
    pub(crate) fn reconnect_receiver(&mut self) -> Result<()> {
        self.refresh_receiver()?;
        if let Some(service) = &self.service {
            service.reconnect().map_err(native_error)?;
        }
        Ok(())
    }
    pub(crate) fn cancel_reception(&mut self, id: Uuid) -> Result<()> {
        if !self.store.received_fax(id)?.outcome.is_active() {
            return Err(Error::Invalid("Reception is no longer active".into()));
        }
        self.service
            .as_ref()
            .ok_or(Error::WorkerStopped)?
            .cancel_incoming(id)
            .map_err(native_error)
    }
    pub(crate) fn retry_export(&mut self, id: Uuid) -> Result<()> {
        let fax = self.store.received_fax(id)?;
        if fax.outcome.is_active() {
            return Err(Error::Invalid("Wait for reception to finish".into()));
        }
        if !matches!(
            fax.export,
            ExportStatus::Published { .. } | ExportStatus::NoContent
        ) && self.exporting != Some(id)
            && !self.exports.contains(&id)
        {
            self.exports.push_back(id);
        }
        Ok(())
    }
    pub(crate) fn clear_queue(&mut self) -> Result<usize> {
        for (&id, (_, cancellation)) in &self.active {
            cancellation.cancel();
            self.clear_active.insert(id);
        }
        let count = self.store.clear_queue()?;
        self.view.jobs = Default::default();
        self.dirty = true;
        Ok(count)
    }
    pub(crate) fn enqueue(&mut self, mut request: FaxRequest) -> Result<Job> {
        request.document = self.documents.load(request.document.id)?;
        let job = self.store.enqueue(request, None)?;
        self.job_changed(job.clone());
        Ok(job)
    }
    pub(crate) fn retry(&mut self, id: Uuid) -> Result<Job> {
        let previous = self.store.job(id)?;
        if !previous.state.is_finished() {
            return Err(Error::Invalid(
                "Only a finished or interrupted fax can be retried".into(),
            ));
        }
        let mut request = previous.request;
        request.document = self.documents.load(request.document.id)?;
        let job = self.store.enqueue(request, Some(id))?;
        self.job_changed(job.clone());
        Ok(job)
    }
    pub(crate) fn cancel(&mut self, id: Uuid) -> Result<()> {
        if let Some((_, cancellation)) = self.active.get(&id) {
            cancellation.cancel();
            return Ok(());
        }
        let mut job = self.store.job(id)?;
        if !matches!(job.state, JobState::Queued) {
            return Err(Error::Invalid(
                "This fax is no longer queued or active".into(),
            ));
        }
        job.state = JobState::Cancelled {
            acknowledged_pages: 0,
        };
        self.store.update_job(&mut job)?;
        self.job_changed(job);
        Ok(())
    }
    fn refresh_receiver(&mut self) -> Result<()> {
        self.generation += 1;
        let generation = self.generation;
        let Some(service) = self.service.clone() else {
            return Ok(());
        };
        let settings = self.view.receive_settings.clone();
        if let Some(listener) = &settings.listener {
            let profile = SipProfile {
                id: Uuid::nil(),
                name: "Direct listener".into(),
                server: listener.bind.ip().to_string(),
                port: listener.bind.port().max(1),
                transport: match listener.transport {
                    SignalingTransport::Udp => SipTransport::Udp,
                    SignalingTransport::Tcp => SipTransport::Tcp,
                    SignalingTransport::Tls => SipTransport::Tls,
                },
                username: "faxe".into(),
                auth_username: None,
                register: false,
                outbound_proxy: None,
                station_id: "FAXE".into(),
                sending_mode: settings.mode,
                automatic_nat: false,
                stun_server: None,
                audio_playout_delay_ms: 200,
            };
            return self.install_receiver(service, profile, settings, None);
        }
        let Some(id) = settings.profile else {
            return service.configure(None).map_err(native_error);
        };
        settings.validate(&self.view.profiles)?;
        if self.configuring {
            return Ok(());
        }
        let profile = self.store.profile(id)?;
        let credentials = self.credentials.clone();
        self.configuring = true;
        let result = self.spawn("faxe-credentials", move || {
            let password = credentials.password(id);
            Box::new(move |a| {
                a.configuring = false;
                if a.stopping {
                    return;
                }
                if a.generation != generation {
                    if let Err(error) = a.refresh_receiver() {
                        a.error(error);
                    }
                    return;
                }
                let result = password
                    .and_then(|password| a.install_receiver(service, profile, settings, password));
                if let Err(error) = result {
                    a.view.receiver_status = ReceiverStatus {
                        profile: Some(id),
                        state: ReceiverState::Error {
                            reason: error.to_string(),
                        },
                    };
                    a.error(error);
                }
            })
        });
        if result.is_err() {
            self.configuring = false;
        }
        result
    }
    fn install_receiver(
        &mut self,
        service: faxe_native::SipService,
        profile: SipProfile,
        settings: ReceiveSettings,
        password: Option<Password>,
    ) -> Result<()> {
        let completion = self.completion.clone();
        let profile_name = profile.name.clone();
        let profile_id = settings.profile;
        let options = settings.clone();
        let config = faxe_native::ReceiveConfig {
            account: crate::sender::account(&profile, password.as_ref().map(Password::expose)),
            station_id: profile.station_id,
            mode: crate::sender::mode(settings.mode),
            ecm: settings.ecm,
            offer: Arc::new(move |request| {
                let pending = PendingIncoming {
                    request,
                    profile_id,
                    profile_name: profile_name.clone(),
                    options: options.clone(),
                };
                let _ = completion.send(Internal::Message(Box::new(move |a| {
                    a.offer_incoming(pending)
                })));
            }),
        };
        if let Some(listener) = settings.listener {
            service
                .configure_listener(config, listener)
                .map_err(native_error)
        } else {
            service.configure(Some(config)).map_err(native_error)
        }
    }
    fn offer_incoming(&mut self, pending: PendingIncoming) {
        self.admissions.retain(|_, p| p.request.is_pending());
        if self.stopping || !pending.request.is_pending() {
            let _ = pending.request.reject();
            return;
        }
        let id = pending.request.id;
        let manual = pending.options.manual;
        let offer = IncomingFax {
            id,
            caller: pending.request.caller.clone(),
            destination: pending.request.destination.clone(),
            peer: pending.request.peer.clone(),
            arrived_at: pending.request.arrived.into(),
        };
        self.admissions.insert(id, pending);
        if manual {
            if self.incoming.try_send(offer).is_err() {
                let _ = self.reject_incoming(id);
            }
        } else if let Err(error) = self.accept_incoming(id) {
            self.error(error);
        }
    }
    fn reject_incoming(&mut self, id: Uuid) -> Result<()> {
        let pending = self
            .admissions
            .remove(&id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        if !pending.request.is_pending() {
            return Err(Error::Invalid("Incoming offer expired".into()));
        }
        pending.request.reject().map_err(native_error)
    }
    fn accept_incoming(&mut self, id: Uuid) -> Result<ReceivedFax> {
        let pending = self
            .admissions
            .remove(&id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        if !pending.request.is_pending() {
            return Err(Error::Invalid("Incoming offer expired".into()));
        }
        let request = pending.request;
        let result = self.reserve_incoming(
            IncomingFax {
                id,
                caller: request.caller.clone(),
                arrived_at: request.arrived.into(),
                destination: request.destination.clone(),
                peer: request.peer.clone(),
            },
            pending.profile_id,
            pending.profile_name,
            pending.options,
        );
        match result {
            Ok(path) => {
                if request.respond(Ok(path)).is_err() {
                    let mut fax = self.store.received_fax(id)?;
                    fax.outcome = ReceptionOutcome::Interrupted;
                    fax.result = Some(ReceptionResult::Interrupted);
                    fax.finished_at = Some(chrono::Utc::now());
                    self.store.save_received(&fax)?;
                    self.received_changed(fax);
                    self.retry_export(id)?;
                }
                self.store.received_fax(id)
            }
            Err(error) => {
                let _ = request.respond(Err(faxe_native::Error::Invalid(error.to_string())));
                Err(error)
            }
        }
    }
    fn reserve_incoming(
        &mut self,
        incoming: IncomingFax,
        profile_id: Option<Uuid>,
        profile_name: String,
        options: ReceiveSettings,
    ) -> Result<PathBuf> {
        if self.stopping {
            return Err(Error::WorkerStopped);
        }
        let IncomingFax {
            id,
            caller,
            arrived_at,
            destination,
            peer,
        } = incoming;
        let spool = self.directory.join("inbox").join(id.to_string());
        std::fs::create_dir_all(&spool)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o700))?;
        }
        let fax = ReceivedFax {
            id,
            profile_id,
            profile_name,
            caller,
            destination,
            peer,
            tiff_path: Some(spool.join("fax.tiff")),
            arrived_at,
            filename_stem: arrived_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d_%H-%M-%S")
                .to_string(),
            finished_at: None,
            options,
            confirmed_pages: 0,
            recovered_pages: 0,
            transport: None,
            outcome: ReceptionOutcome::Receiving,
            result: None,
            export: ExportStatus::Pending,
            partial_page_preservation_available: true,
            recovery: None,
        };
        self.store.save_received(&fax)?;
        self.received_changed(fax);
        Ok(spool.join("fax.tiff"))
    }
    fn queue_settings(
        &mut self,
        settings: Settings,
        reply: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        if let Some((latest, replies)) = &mut self.settings_pending {
            if replies.len() >= 64 {
                return Err(Error::Invalid("Settings queue is full".into()));
            }
            *latest = settings;
            replies.push(reply);
        } else {
            self.settings_pending = Some((settings, vec![reply]));
        }
        self.start_settings()
    }
    fn start_settings(&mut self) -> Result<()> {
        if self.settings_saving {
            return Ok(());
        }
        let Some((settings, replies)) = self.settings_pending.take() else {
            return Ok(());
        };
        self.settings_saving = true;
        let result = self.spawn("faxe-settings", move || {
            let result = settings.save().map_err(|e| e.to_string());
            Box::new(move |actor| {
                actor.settings_saving = false;
                for reply in replies {
                    let _ = reply.send(
                        result
                            .as_ref()
                            .map(|_| ())
                            .map_err(|e| Error::Invalid(e.clone())),
                    );
                }
                // Flush accepted preferences even during shutdown, in submission order.
                if let Err(error) = actor.start_settings() {
                    actor.error(error);
                }
            })
        });
        if result.is_err() {
            self.settings_saving = false;
        }
        result
    }
    fn start_send(&mut self) -> Result<()> {
        if self.active.len() >= self.options.limits.outgoing
            || !self
                .view
                .jobs
                .iter()
                .any(|job| matches!(job.state, JobState::Queued))
        {
            return Ok(());
        }
        let Some(backend) = self.backend.clone() else {
            return Ok(());
        };
        let Some(job) = self.store.claim_next()? else {
            return Ok(());
        };
        let cancellation = Cancellation::default();
        self.active
            .insert(job.id, (job.clone(), cancellation.clone()));
        self.job_changed(job.clone());
        let credentials = self.credentials.clone();
        let path = self.documents.fax_path(job.request.document.id);
        let send = self.completion.clone();
        let id = job.id;
        let result = self.spawn("faxe-transmit", move || {
            let id = job.id;
            let outcome = (|| {
                let password = credentials.password(job.profile.id)?;
                cancellation.check()?;
                backend.send(
                    OutgoingFax {
                        job: &job,
                        tiff_path: &path,
                        password: password.as_ref(),
                    },
                    &cancellation,
                    &mut |progress| {
                        let cancellation = cancellation.clone();
                        let (reply, ack) = channel::bounded(1);
                        if send
                            .send(Internal::Message(Box::new(move |a| {
                                if let Err(error) = a.progress(id, progress) {
                                    cancellation.cancel();
                                    a.error(error);
                                }
                                let _ = reply.send(());
                            })))
                            .is_ok()
                        {
                            let _ = ack.recv();
                        }
                    },
                )
            })();
            Box::new(move |a| {
                if let Err(error) = a.sent(id, outcome) {
                    a.error(error);
                }
            })
        });
        if let Err(error) = result {
            self.sent(id, Err(error))?;
        }
        Ok(())
    }
    fn start_export(&mut self) -> Result<()> {
        if self.exporting.is_some() {
            return Ok(());
        }
        let Some(id) = self.exports.pop_front() else {
            return Ok(());
        };
        let mut fax = self.store.received_fax(id)?;
        let spool = self.directory.join("inbox").join(id.to_string());
        let documents = self.documents.clone();
        let send = self.completion.clone();
        self.exporting = Some(id);
        let result = self.spawn("faxe-export", move || {
            let result = crate::export::export(&documents, &spool, &mut fax, |fax| {
                let fax = fax.clone();
                let (reply, ack) = channel::bounded(1);
                send.send(Internal::Message(Box::new(move |a| {
                    let result = a.store.save_received(&fax);
                    if result.is_ok() {
                        a.received_changed(fax);
                    }
                    let _ = reply.send(result);
                })))
                .map_err(|_| Error::WorkerStopped)?;
                ack.recv().map_err(|_| Error::WorkerStopped)?
            });
            Box::new(move |a| {
                a.exporting = None;
                if let Err(error) = result {
                    match &mut fax.export {
                        ExportStatus::Publishing { error: detail, .. } => {
                            *detail = Some(error.to_string())
                        }
                        _ => {
                            fax.export = ExportStatus::Failed {
                                reason: error.to_string(),
                            }
                        }
                    }
                    if let Err(save) = a.store.save_received(&fax) {
                        a.error(save);
                    } else {
                        a.received_changed(fax);
                    }
                    a.error(error);
                }
            })
        });
        if result.is_err() {
            self.exporting = None;
        }
        result
    }
}
fn upsert<T>(items: &mut Vec<T>, value: T, id: impl Fn(&T) -> Uuid) {
    if let Some(item) = items.iter_mut().find(|item| id(item) == id(&value)) {
        *item = value;
    } else {
        items.insert(0, value);
    }
}
fn native_error(error: faxe_native::Error) -> Error {
    Error::Transmission(error.to_string())
}

impl Actor {
    fn progress(&mut self, id: Uuid, progress: TransmissionProgress) -> Result<()> {
        let activity_only = matches!(
            progress,
            TransmissionProgress::T38Activity { .. } | TransmissionProgress::PageProgress(_)
        );
        let Some((job, _)) = self.active.get_mut(&id) else {
            return Ok(());
        };
        let previous = job.clone();
        if let TransmissionProgress::Stage(stage) = progress
            && job.steps.last().is_none_or(|step| step.stage != stage)
        {
            job.steps.push(crate::JobStep {
                stage,
                at: chrono::Utc::now(),
            });
        }
        job.state = match progress {
            TransmissionProgress::Stage(stage) if job.state.acknowledged_pages() == 0 => {
                match stage {
                    FaxStage::Connecting
                    | FaxStage::Securing
                    | FaxStage::Registering
                    | FaxStage::Calling
                    | FaxStage::Ringing => JobState::Connecting,
                    FaxStage::SendingT38 | FaxStage::SendingG711 | FaxStage::Confirming => {
                        JobState::Sending {
                            acknowledged_pages: 0,
                        }
                    }
                    FaxStage::Connected
                    | FaxStage::OfferingT38
                    | FaxStage::FallingBack
                    | FaxStage::Negotiating => JobState::Negotiating,
                }
            }
            TransmissionProgress::Stage(_) => job.state.clone(),
            TransmissionProgress::T38Activity { transmitted_bytes } => {
                job.transmitted_bytes = job.transmitted_bytes.max(transmitted_bytes);
                job.state.clone()
            }
            TransmissionProgress::PageProgress(page) => {
                if page.total_rows > 0
                    && page.rows <= page.total_rows
                    && job.page_progress.is_none_or(|old| {
                        page.page > old.page
                            || (page.page == old.page
                                && page.rows as u64 * old.total_rows as u64
                                    >= old.rows as u64 * page.total_rows as u64)
                    })
                {
                    job.page_progress = Some(page);
                }
                job.state.clone()
            }
            TransmissionProgress::PageAcknowledged(acknowledged_pages) => {
                if acknowledged_pages >= job.request.document.pages
                    && job
                        .steps
                        .last()
                        .is_none_or(|step| step.stage != crate::FaxStage::Confirming)
                {
                    job.steps.push(crate::JobStep {
                        stage: crate::FaxStage::Confirming,
                        at: chrono::Utc::now(),
                    });
                }
                JobState::Sending {
                    acknowledged_pages: acknowledged_pages.max(job.state.acknowledged_pages()),
                }
            }
        };

        if *job == previous {
            return Ok(());
        }
        if !self.clear_active.contains(&id) && !activity_only {
            self.store.update_job(job)?;
        }
        let job = job.clone();
        if !self.clear_active.contains(&id) {
            self.job_changed(job);
        }
        Ok(())
    }
    fn sent(&mut self, id: Uuid, outcome: Result<Receipt>) -> Result<()> {
        let Some((mut job, _)) = self.active.remove(&id) else {
            return Ok(());
        };
        let acknowledged_pages = job.state.acknowledged_pages();
        job.state = match outcome {
            Ok(receipt) if receipt.acknowledged_pages == job.request.document.pages => {
                JobState::Succeeded {
                    acknowledged_pages: receipt.acknowledged_pages,
                }
            }
            Ok(receipt) => JobState::Failed {
                reason: "T.30 completed with an unexpected page count".into(),
                acknowledged_pages: receipt.acknowledged_pages,
            },
            Err(Error::Cancelled) => JobState::Cancelled { acknowledged_pages },
            Err(error) => {
                tracing::error!(job_id = %job.id, %error, acknowledged_pages, "Fax job failed");
                JobState::Failed {
                    reason: error.to_string(),
                    acknowledged_pages,
                }
            }
        };

        if self.stopping && matches!(job.state, JobState::Cancelled { .. }) {
            job.state = JobState::Interrupted;
        }
        if self.clear_active.remove(&id) {
            self.store.remove_job(job.id)?;
        } else {
            self.store.update_job(&mut job)?;
            self.job_changed(job);
        }
        Ok(())
    }
    fn received(&mut self, event: faxe_native::ReceiveEvent) -> Result<()> {
        use faxe_native::ReceiveEvent;
        match event {
            ReceiveEvent::Status { profile, state } => {
                let status = crate::ReceiverStatus {
                    profile,
                    state: match state {
                        faxe_native::ReceiveState::Off => crate::ReceiverState::Off,
                        faxe_native::ReceiveState::Registering => crate::ReceiverState::Registering,
                        faxe_native::ReceiveState::Ready => crate::ReceiverState::Ready,
                        faxe_native::ReceiveState::Receiving => crate::ReceiverState::Receiving,
                        faxe_native::ReceiveState::Error(reason) => {
                            crate::ReceiverState::Error { reason }
                        }
                    },
                };
                if self.view.receiver_status != status {
                    self.view.receiver_status = status.clone();
                    self.emit(EngineEvent::ReceiverStatus(status));
                }
            }
            ReceiveEvent::Progress { id, event } => {
                let mut fax = self.store.received_fax(id)?;
                match event {
                    faxe_native::FaxEvent::PageAcknowledged(stats) => {
                        fax.confirmed_pages = fax.confirmed_pages.max(stats.received_pages)
                    }
                    faxe_native::FaxEvent::Stage(crate::FaxStage::SendingT38) => {
                        fax.transport = Some(crate::FaxMode::T38)
                    }
                    faxe_native::FaxEvent::Stage(crate::FaxStage::SendingG711) => {
                        fax.transport = Some(crate::FaxMode::G711)
                    }
                    _ => return Ok(()),
                }
                self.store.save_received(&fax)?;
                self.received_changed(fax);
            }
            ReceiveEvent::Finished {
                id,
                outcome,
                transport,
                recovery,
            } => {
                let mut fax = self.store.received_fax(id)?;
                fax.finished_at = Some(chrono::Utc::now());
                fax.transport = transport.map(|mode| match mode {
                    faxe_native::Mode::T38 => crate::FaxMode::T38,
                    faxe_native::Mode::G711 => crate::FaxMode::G711,
                    faxe_native::Mode::Auto => crate::FaxMode::Auto,
                });
                fax.result = Some(match &outcome {
                    Ok(_) => crate::ReceptionResult::Succeeded,
                    Err(faxe_native::Error::Cancelled) if self.stopping => {
                        crate::ReceptionResult::Interrupted
                    }
                    Err(error) => crate::ReceptionResult::Failed {
                        reason: error.to_string(),
                        t30_code: if let faxe_native::Error::Fax(failure) = error {
                            Some(failure.code)
                        } else {
                            None
                        },
                    },
                });
                fax.outcome = match outcome {
                    Ok(stats) => {
                        fax.confirmed_pages = stats.received_pages;
                        if stats.received_pages > 0 {
                            crate::ReceptionOutcome::Received
                        } else {
                            crate::ReceptionOutcome::Failed {
                                reason: "Call completed without a received page".into(),
                            }
                        }
                    }
                    Err(faxe_native::Error::Cancelled) if self.stopping => {
                        crate::ReceptionOutcome::Interrupted
                    }
                    Err(error) => {
                        if let faxe_native::Error::Fax(failure) = &error {
                            fax.confirmed_pages =
                                fax.confirmed_pages.max(failure.stats.received_pages);
                        }
                        crate::ReceptionOutcome::Failed {
                            reason: error.to_string(),
                        }
                    }
                };
                if let Some(report) = &recovery {
                    fax.confirmed_pages = report.confirmed_complete_pages;
                    if let Some(reason) = report.incomplete_reason()
                        && fax.outcome.is_complete()
                    {
                        fax.outcome = crate::ReceptionOutcome::Failed { reason };
                    }
                    if matches!(fax.result, Some(crate::ReceptionResult::Succeeded)) {
                        fax.result = Some(match report.completion {
                            crate::ReceiveCompletion::Completed { code: 0 } => {
                                crate::ReceptionResult::Succeeded
                            }
                            crate::ReceiveCompletion::Completed { code } => {
                                crate::ReceptionResult::Failed {
                                    reason: format!("T.30 failed with code {code}"),
                                    t30_code: Some(code),
                                }
                            }
                            crate::ReceiveCompletion::Interrupted { .. } => {
                                crate::ReceptionResult::Interrupted
                            }
                        });
                    }
                }
                fax.recovery = recovery;
                self.store.save_received(&fax)?;
                self.received_changed(fax);
                self.retry_export(id)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod reception_effect_tests {
    use super::*;

    struct NoCredentials;
    impl Credentials for NoCredentials {
        fn password(&self, _: Uuid) -> Result<Option<Password>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn received_pdf_opens_once_after_publication_even_with_notifications_disabled()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut runtime =
            EngineRuntime::open(directory.path().into(), Arc::new(NoCredentials), None)?;
        let handle = runtime.handle();
        let mut effects = runtime.take_effects().unwrap();
        let id = Uuid::new_v4();
        handle
            .request(move |actor| {
                actor.reserve_incoming(
                    IncomingFax {
                        id,
                        caller: "Caller".into(),
                        arrived_at: chrono::Utc::now(),
                        destination: String::new(),
                        peer: String::new(),
                    },
                    Some(Uuid::new_v4()),
                    "Fixture".into(),
                    ReceiveSettings {
                        notifications: false,
                        ..Default::default()
                    },
                )?;
                let mut fax = actor.store.received_fax(id)?;
                fax.outcome = ReceptionOutcome::Received;
                for export in [
                    ExportStatus::NoContent,
                    ExportStatus::Failed {
                        reason: "Folder unavailable".into(),
                    },
                ] {
                    fax.export = export;
                    actor.received_changed(fax.clone());
                }
                actor.store.save_received(&fax)
            })
            .await?;
        assert!(effects.try_recv().is_err(), "no PDF exists to open yet");

        let path = directory.path().join("received fax.pdf");
        let published_path = path.clone();
        handle
            .request(move |actor| {
                let mut fax = actor.store.received_fax(id)?;
                fax.export = ExportStatus::Published {
                    path: published_path,
                };
                actor.store.save_received(&fax)?;
                actor.received_changed(fax.clone());
                actor.received_changed(fax.clone());
                fax.caller = "Updated caller".into();
                actor.store.save_received(&fax)?;
                actor.received_changed(fax);
                Ok(())
            })
            .await?;
        assert!(
            matches!(effects.try_recv(), Ok(EngineEffect::ReceivedPdfReady { id: received_id, path: received_path }) if received_id == id && received_path == path)
        );
        assert!(
            effects.try_recv().is_err(),
            "duplicate and metadata updates must not reopen the PDF"
        );
        runtime.shutdown().await?;

        let mut reopened =
            EngineRuntime::open(directory.path().into(), Arc::new(NoCredentials), None)?;
        let mut effects = reopened.take_effects().unwrap();
        assert_eq!(reopened.handle().view().inbox.len(), 1);
        assert!(
            effects.try_recv().is_err(),
            "existing inbox PDFs must not open at startup"
        );
        reopened.shutdown().await
    }
}
