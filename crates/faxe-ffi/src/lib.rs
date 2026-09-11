//! Version 1 C ABI. Handles are registry IDs, never pointers into Rust or Go.
//! Requests/results are UTF-8 JSON in owned buffers; see `include/faxe.h`.
#![deny(unsafe_op_in_unsafe_fn)]
use faxe_engine::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

type Registry = Mutex<HashMap<u64, Arc<Client>>>;
fn registry() -> &'static Registry {
    static CLIENTS: OnceLock<Registry> = OnceLock::new();
    CLIENTS.get_or_init(Default::default)
}
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Deserialize)]
struct AccountConfig {
    profile: SipProfile,
    #[serde(default)]
    password: Option<String>,
}
#[derive(Deserialize)]
struct OpenConfig {
    data_dir: PathBuf,
    #[serde(default = "EngineOptions::server")]
    options: EngineOptions,
    #[serde(default)]
    accounts: Vec<AccountConfig>,
    #[serde(default)]
    receiver: Option<ReceiveSettings>,
}
#[derive(Default)]
struct MemoryCredentials(Mutex<HashMap<Uuid, String>>);
impl Credentials for MemoryCredentials {
    fn password(&self, id: Uuid) -> Result<Option<Password>> {
        Ok(lock(&self.0).get(&id).cloned().map(Password::new))
    }
    fn save_password(&self, id: Uuid, password: &Password) -> Result<()> {
        lock(&self.0).insert(id, password.expose().into());
        Ok(())
    }
}
struct Operation {
    cancellation: Cancellation,
    job: Mutex<Option<Uuid>>,
}
struct Reader {
    view: watch::Receiver<Arc<EngineView>>,
    incoming: mpsc::Receiver<IncomingFax>,
    jobs: HashMap<Uuid, Value>,
    receptions: HashMap<Uuid, Value>,
    status: Value,
    error: Option<String>,
}
struct Client {
    runtime: tokio::runtime::Runtime,
    engine: Mutex<Option<EngineRuntime>>,
    handle: EngineHandle,
    reader: Mutex<Reader>,
    operations: Mutex<HashMap<Uuid, Arc<Operation>>>,
    closed: AtomicBool,
}
impl Client {
    fn open(config: OpenConfig) -> Result<Self> {
        config.options.validate()?;
        let credentials = Arc::new(MemoryCredentials::default());
        for account in &config.accounts {
            account.profile.validate()?;
            if let Some(password) = &account.password {
                lock(&credentials.0).insert(account.profile.id, password.clone());
            }
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let sender = Arc::new(SipSender::with_options(&config.options)?);
        let mut engine = EngineRuntime::open_with_options(
            std::path::absolute(config.data_dir)?,
            credentials,
            Some(sender),
            config.options,
        )?;
        let handle = engine.handle();
        let setup = runtime.block_on(async {
            for account in config.accounts {
                handle.save_profile(account.profile).await?;
            }
            if let Some(settings) = config.receiver {
                handle.configure_receiver(settings).await?;
            }
            Ok::<_, Error>(())
        });
        if let Err(error) = setup {
            let _ = engine.shutdown_blocking();
            return Err(error);
        }
        let view = handle.view();
        let reader = Reader {
            view: handle.observe(),
            incoming: engine.take_incoming().unwrap(),
            jobs: view.jobs.iter().map(|j| (j.id, job_value(j))).collect(),
            receptions: view
                .inbox
                .iter()
                .map(|f| (f.id, reception_value(f)))
                .collect(),
            status: Value::Null,
            error: None,
        };
        Ok(Self {
            runtime,
            engine: Mutex::new(Some(engine)),
            handle,
            reader: Mutex::new(reader),
            operations: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        })
    }
    fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        for op in lock(&self.operations).values() {
            op.cancellation.cancel();
        }
        if let Some(mut engine) = lock(&self.engine).take() {
            engine.shutdown_blocking()?;
        }
        Ok(())
    }
    fn call(&self, request: Request) -> Result<Value> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::WorkerStopped);
        }
        self.runtime.block_on(async {
            match request {
                Request::NewOperation => {
                    let id = Uuid::new_v4();
                    lock(&self.operations).insert(
                        id,
                        Arc::new(Operation {
                            cancellation: Cancellation::default(),
                            job: Mutex::new(None),
                        }),
                    );
                    Ok(json!({"id": id}))
                }
                Request::CancelOperation { id } => {
                    let op = lock(&self.operations).get(&id).cloned();
                    if let Some(op) = op {
                        op.cancellation.cancel();
                        let job = *lock(&op.job);
                        if let Some(id) = job {
                            let _ = self.handle.cancel(id).await;
                        }
                    }
                    Ok(Value::Null)
                }
                Request::ReleaseOperation { id } => {
                    lock(&self.operations).remove(&id);
                    Ok(Value::Null)
                }
                Request::Submit {
                    operation,
                    profile_id,
                    destination,
                    paths,
                    mode,
                    options,
                } => {
                    let op = lock(&self.operations)
                        .get(&operation)
                        .cloned()
                        .ok_or_else(|| Error::NotFound(operation.to_string()))?;
                    let document = self
                        .handle
                        .prepare(DocumentInput { paths, options }, op.cancellation.clone())
                        .await?;
                    if op.cancellation.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    let job = self
                        .handle
                        .enqueue(FaxRequest {
                            profile_id,
                            destination,
                            mode,
                            document,
                        })
                        .await?;
                    *lock(&op.job) = Some(job.id);
                    if op.cancellation.is_cancelled() {
                        let _ = self.handle.cancel(job.id).await;
                        return Err(Error::Cancelled);
                    }
                    Ok(job_value(&job))
                }
                Request::Job { id } => Ok(job_value(&self.handle.job(id).await?)),
                Request::Cancel { id } => {
                    self.handle.cancel(id).await?;
                    Ok(Value::Null)
                }
                Request::Reception { id } => Ok(reception_value(&self.handle.reception(id).await?)),
                Request::Accept { id } => {
                    Ok(reception_value(&self.handle.accept_incoming(id).await?))
                }
                Request::Reject { id } => {
                    self.handle.reject_incoming(id).await?;
                    Ok(Value::Null)
                }
                Request::CancelReception { id } => {
                    self.handle.cancel_reception(id).await?;
                    Ok(Value::Null)
                }
                Request::ConfigureReceiver { settings } => {
                    self.handle.configure_receiver(settings).await?;
                    Ok(Value::Null)
                }
                Request::RetryExport { id } => {
                    self.handle.retry_export(id).await?;
                    Ok(Value::Null)
                }
                Request::ReceiverStatus => Ok(status_value(&self.handle.view().receiver_status)),
            }
        })
    }
    fn poll(&self, timeout_ms: u32) -> Result<Value> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::WorkerStopped);
        }
        let mut reader = lock(&self.reader);
        self.runtime.block_on(async {
            let mut offers = Vec::new();
            while let Ok(offer) = reader.incoming.try_recv() { offers.push(offer); }
            if offers.is_empty() && !reader.view.has_changed().unwrap_or(true) && !reader.status.is_null() {
                let Reader { view, incoming, .. } = &mut *reader;
                tokio::select! {
                    _ = view.changed() => (),
                    offer = incoming.recv() => if let Some(offer) = offer { offers.push(offer); },
                    _ = tokio::time::sleep(Duration::from_millis(timeout_ms.min(1000) as u64)) => (),
                }
            }
            if self.closed.load(Ordering::Acquire) { return Err(Error::WorkerStopped); }
            let view = reader.view.borrow_and_update().clone();
            let mut events = Vec::new();
            for offer in offers { events.push(json!({"kind": "incoming", "value": offer})); }
            for job in view.jobs.iter() {
                let value = job_value(job);
                if reader.jobs.get(&job.id) != Some(&value) {
                    reader.jobs.insert(job.id, value.clone());
                    events.push(json!({"kind": "job", "value": value}));
                }
            }
            for fax in view.inbox.iter() {
                let value = reception_value(fax);
                if reader.receptions.get(&fax.id) != Some(&value) {
                    reader.receptions.insert(fax.id, value.clone());
                    events.push(json!({"kind": "reception", "value": value}));
                }
            }
            let status = status_value(&view.receiver_status);
            if reader.status != status {
                reader.status = status.clone();
                events.push(json!({"kind": "receiver", "value": status}));
            }
            if reader.error != view.error {
                reader.error = view.error.clone();
                if let Some(error) = &view.error { events.push(json!({"kind": "error", "value": error})); }
            }
            Ok(json!(events))
        })
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    NewOperation,
    CancelOperation {
        id: Uuid,
    },
    ReleaseOperation {
        id: Uuid,
    },
    Submit {
        operation: Uuid,
        profile_id: Uuid,
        destination: String,
        paths: Vec<PathBuf>,
        #[serde(default)]
        mode: FaxMode,
        #[serde(default)]
        options: DocumentOptions,
    },
    Job {
        id: Uuid,
    },
    Cancel {
        id: Uuid,
    },
    Reception {
        id: Uuid,
    },
    Accept {
        id: Uuid,
    },
    Reject {
        id: Uuid,
    },
    CancelReception {
        id: Uuid,
    },
    ConfigureReceiver {
        settings: ReceiveSettings,
    },
    RetryExport {
        id: Uuid,
    },
    ReceiverStatus,
}
fn status_value(status: &ReceiverStatus) -> Value {
    let (state, error) = match &status.state {
        ReceiverState::Off => ("off", None),
        ReceiverState::Registering => ("registering", None),
        ReceiverState::Ready => ("ready", None),
        ReceiverState::Receiving => ("receiving", None),
        ReceiverState::Error { reason } => ("error", Some(reason)),
    };
    json!({"profile_id": status.profile, "state": state, "error": error})
}
fn job_value(job: &Job) -> Value {
    let (state, error) = match &job.state {
        JobState::Queued => ("queued", None),
        JobState::Connecting => ("connecting", None),
        JobState::Negotiating => ("negotiating", None),
        JobState::Sending { .. } => ("sending", None),
        JobState::Succeeded { .. } => ("succeeded", None),
        JobState::Failed { reason, .. } => ("failed", Some(reason)),
        JobState::Cancelled { .. } => ("cancelled", None),
        JobState::Interrupted => ("interrupted", None),
    };
    json!({"id": job.id, "profile_id": job.profile.id, "destination": job.request.destination,
        "state": state, "error": error, "pages": job.request.document.pages,
        "acknowledged_pages": job.state.acknowledged_pages(), "done": job.state.is_finished(),
        "mode": job.request.mode, "created_at": job.created_at, "updated_at": job.updated_at,
        "transmitted_bytes": job.transmitted_bytes, "page_progress": job.page_progress})
}
fn reception_value(fax: &ReceivedFax) -> Value {
    let (state, error) = match &fax.outcome {
        ReceptionOutcome::Receiving => ("receiving", None),
        ReceptionOutcome::Received => ("received", None),
        ReceptionOutcome::Partial { reason } => ("partial", Some(reason)),
        ReceptionOutcome::Failed { reason } => ("failed", Some(reason)),
        ReceptionOutcome::Interrupted => ("interrupted", None),
    };
    let (export, pdf, export_error) = match &fax.export {
        ExportStatus::Pending => ("pending", None, None),
        ExportStatus::Publishing {
            error: Some(error), ..
        } => ("failed", None, Some(error)),
        ExportStatus::Publishing { .. } => ("publishing", None, None),
        ExportStatus::Published { path } => ("published", Some(path), None),
        ExportStatus::Failed { reason } => ("failed", None, Some(reason)),
        ExportStatus::NoContent => ("no_content", None, None),
    };
    let (result, t30_code) = match &fax.result {
        Some(ReceptionResult::Succeeded) => ("succeeded", None),
        Some(ReceptionResult::Failed { t30_code, .. }) => ("failed", *t30_code),
        Some(ReceptionResult::Interrupted) => ("interrupted", None),
        None => ("pending", None),
    };
    json!({"id": fax.id, "profile_id": fax.profile_id, "caller": fax.caller,
        "destination": fax.destination, "peer": fax.peer, "arrived_at": fax.arrived_at, "finished_at": fax.finished_at,
        "confirmed_pages": fax.confirmed_pages, "recovered_pages": fax.recovered_pages,
        "transport": fax.transport, "state": state, "result": result, "t30_code": t30_code,
        "error": error, "export_state": export, "pdf_path": pdf, "tiff_path": fax.tiff_path,
        "export_error": export_error, "done": fax.is_finished()})
}
fn client(id: u64) -> Result<Arc<Client>> {
    lock(registry())
        .get(&id)
        .cloned()
        .ok_or(Error::WorkerStopped)
}

#[repr(C)]
pub struct FaxeBuffer {
    pub data: *mut u8,
    pub len: usize,
}
fn buffer(value: Value) -> FaxeBuffer {
    let bytes = serde_json::to_vec(&value)
        .expect("JSON value serialization")
        .into_boxed_slice();
    let len = bytes.len();
    FaxeBuffer {
        data: Box::into_raw(bytes).cast(),
        len,
    }
}
fn boundary(f: impl FnOnce() -> Result<Value>) -> FaxeBuffer {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(value)) => buffer(json!({"ok": true, "result": value})),
        Ok(Err(error)) => {
            let code = match error {
                Error::WorkerStopped => "closed",
                Error::Cancelled => "cancelled",
                Error::NotFound(_) => "not_found",
                Error::Invalid(_) | Error::Json(_) => "invalid",
                _ => "engine",
            };
            buffer(json!({"ok": false, "error": {"code": code, "message": error.to_string()}}))
        }
        Err(_) => buffer(
            json!({"ok": false, "error": {"code": "panic", "message": "Native operation panicked"}}),
        ),
    }
}
unsafe fn input<T: serde::de::DeserializeOwned>(data: *const u8, len: usize) -> Result<T> {
    if data.is_null() || len == 0 || len > 1024 * 1024 {
        return Err(Error::Invalid(
            "Expected 1–1048576 bytes of UTF-8 JSON".into(),
        ));
    }
    Ok(serde_json::from_slice(unsafe {
        std::slice::from_raw_parts(data, len)
    })?)
}
#[unsafe(no_mangle)]
pub extern "C" fn faxe_abi_version() -> u32 {
    1
}
/// # Safety
/// `data` must point to `len` readable bytes for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faxe_open(data: *const u8, len: usize) -> FaxeBuffer {
    boundary(|| {
        let client = Arc::new(Client::open(unsafe { input(data, len) }?)?);
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        lock(registry()).insert(id, client);
        Ok(json!({"handle": id}))
    })
}
/// # Safety
/// `data` must point to `len` readable bytes for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faxe_call(handle: u64, data: *const u8, len: usize) -> FaxeBuffer {
    boundary(|| client(handle)?.call(unsafe { input(data, len) }?))
}
#[unsafe(no_mangle)]
pub extern "C" fn faxe_poll(handle: u64, timeout_ms: u32) -> FaxeBuffer {
    boundary(|| client(handle)?.poll(timeout_ms))
}
#[unsafe(no_mangle)]
pub extern "C" fn faxe_close(handle: u64) -> FaxeBuffer {
    boundary(|| {
        let client = lock(registry()).remove(&handle);
        if let Some(client) = client {
            client.close()?;
        }
        Ok(Value::Null)
    })
}
/// # Safety
/// Pass an unmodified buffer returned by this library, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faxe_buffer_free(buffer: FaxeBuffer) {
    if !buffer.data.is_null() {
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                buffer.data,
                buffer.len,
            )));
        }
    }
}
