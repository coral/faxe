//! The service owns every native handle on one thread. Commands contain values only.
use super::*;
use crossbeam_channel as mpsc;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::SystemTime,
};
use uuid::Uuid;

type AdmissionCallback = Arc<dyn Fn(AdmissionRequest) + Send + Sync>;

/// Limits count connecting calls and pending admissions as well as active media.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CallLimits {
    pub outgoing: usize,
    pub incoming: usize,
}
impl Default for CallLimits {
    fn default() -> Self {
        Self {
            outgoing: 1,
            incoming: 1,
        }
    }
}
impl CallLimits {
    pub const SERVER: Self = Self {
        outgoing: 100,
        incoming: 100,
    };
    pub fn validate(self) -> Result<()> {
        if self.outgoing == 0 || self.incoming == 0 {
            return Err(Error::Invalid("Call limits must be positive".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ServiceOptions {
    pub limits: CallLimits,
    pub admission_timeout: Duration,
}
impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            limits: CallLimits::default(),
            admission_timeout: Duration::from_secs(5),
        }
    }
}

/// Direct SIP listening, currently IPv4 UDP or TCP. Wildcard binds require an
/// advertised address so neither Contact nor SDP contains an unspecified IP.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListenerConfig {
    pub bind: std::net::SocketAddr,
    pub transport: SignalingTransport,
    #[serde(default)]
    pub advertised_ip: Option<std::net::IpAddr>,
}
impl ListenerConfig {
    pub fn validate(&self) -> Result<()> {
        let ip = self.advertised_ip.unwrap_or(self.bind.ip());
        if !self.bind.is_ipv4()
            || !ip.is_ipv4()
            || ip.is_unspecified()
            || self.transport == SignalingTransport::Tls
        {
            return Err(Error::Invalid(
                "Direct listeners require IPv4 UDP/TCP and a concrete advertised IP (or bind IP)"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ServiceAccount {
    pub id: Uuid,
    pub server: String,
    pub port: u16,
    pub transport: SignalingTransport,
    pub username: String,
    pub auth_username: Option<String>,
    pub password: Option<String>,
    pub register: bool,
    pub outbound_proxy: Option<String>,
    /// G.711 receive playout delay, 40–1000 ms; normally 200 ms.
    pub audio_playout_delay_ms: u16,
    pub automatic_nat: bool,
    pub stun_server: Option<String>,
}
impl ServiceAccount {
    fn borrowed(&self) -> Account<'_> {
        Account {
            server: &self.server,
            port: self.port,
            transport: self.transport,
            username: &self.username,
            auth_username: self.auth_username.as_deref(),
            password: self.password.as_deref(),
            register: self.register,
            outbound_proxy: self.outbound_proxy.as_deref(),
            audio_playout_delay_ms: self.audio_playout_delay_ms,
        }
    }
}
pub struct ServiceSend {
    pub account: ServiceAccount,
    pub destination: String,
    pub file: PathBuf,
    pub station_id: String,
    pub mode: Mode,
}
#[derive(Clone)]
pub struct ReceiveConfig {
    pub account: ServiceAccount,
    pub station_id: String,
    pub mode: Mode,
    pub ecm: bool,
    /// Enqueue an admission request. This callback must not perform I/O or block.
    pub offer: AdmissionCallback,
}
/// An owned, one-shot admission request; no native handles cross the boundary.
pub struct AdmissionRequest {
    pub id: Uuid,
    pub caller: String,
    pub destination: String,
    pub peer: String,
    pub arrived: SystemTime,
    pending: Arc<AtomicBool>,
    commands: Commands,
}
impl AdmissionRequest {
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
    pub fn respond(self, result: Result<PathBuf>) -> Result<()> {
        self.commands.send(Command::Admit(self.id, result))
    }
    pub fn reject(self) -> Result<()> {
        self.commands.send(Command::Reject(self.id))
    }
}
struct PendingAdmission {
    call: ActiveCall,
    deadline: Instant,
    valid: Arc<AtomicBool>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiveState {
    Off,
    Registering,
    Ready,
    Receiving,
    Error(String),
}
#[derive(Debug)]
pub enum ReceiveEvent {
    Status {
        profile: Option<Uuid>,
        state: ReceiveState,
    },
    Progress {
        id: Uuid,
        event: FaxEvent,
    },
    Finished {
        id: Uuid,
        outcome: Result<TransferStats>,
        transport: Option<Mode>,
        recovery: Option<crate::ReceiveReport>,
    },
}
pub enum SendUpdate {
    Progress(FaxEvent),
    Finished(Result<TransferStats>),
}
enum Command {
    Configure(Option<ReceiveConfig>, Option<ListenerConfig>),
    Send(ServiceSend, Arc<AtomicBool>, mpsc::Sender<SendUpdate>),
    CancelIncoming(Uuid),
    Admit(Uuid, Result<PathBuf>),
    Reject(Uuid),
    Reconnect,
    Stop,
}
#[derive(Clone)]
struct Commands {
    queue: mpsc::Sender<Command>,
    wake: wake::Wake,
}
impl Commands {
    fn send(&self, command: Command) -> Result<()> {
        self.queue
            .send(command)
            .map_err(|_| Error::Sip("SIP service stopped".into()))?;
        self.wake.notify();
        Ok(())
    }
}
struct ServiceOwner {
    commands: Commands,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}
#[derive(Clone)]
pub struct SipService(Arc<ServiceOwner>);
impl SipService {
    pub fn start() -> Result<(Self, mpsc::Receiver<ReceiveEvent>)> {
        Self::start_with_options(ServiceOptions::default())
    }
    pub fn start_with_options(
        options: ServiceOptions,
    ) -> Result<(Self, mpsc::Receiver<ReceiveEvent>)> {
        options.limits.validate()?;
        if options.admission_timeout.is_zero() {
            return Err(Error::Invalid("Admission timeout must be positive".into()));
        }
        let (commands, receiver) = mpsc::unbounded();
        let (ready, started) = mpsc::bounded(1);
        let (events, inbox) = mpsc::unbounded();
        let (wake, reader) = wake::Wake::pair()?;
        let commands = Commands {
            queue: commands,
            wake,
        };
        let worker_commands = commands.clone();
        #[cfg(test)]
        let trust = crate::tls::tests::TRUST.with(|trust| trust.borrow().clone());
        let worker = thread::Builder::new()
            .name("faxe-sip".into())
            .spawn(move || {
                let owner = match ENDPOINT_OWNER.try_lock() {
                    Ok(owner) => owner,
                    Err(_) => {
                        let _ = ready.send(Err(
                            "A native SIP engine is already running in this process".to_string(),
                        ));
                        return;
                    }
                };
                #[cfg(test)]
                crate::tls::tests::TRUST.with(|slot| *slot.borrow_mut() = trust);
                if let Err(error) =
                    service_loop(receiver, &events, worker_commands, reader, options, ready)
                {
                    let _ = events.send(ReceiveEvent::Status {
                        profile: None,
                        state: ReceiveState::Error(error.to_string()),
                    });
                }
                drop(owner);
            })?;
        match started.recv() {
            Ok(Ok(())) => (),
            Ok(Err(error)) => {
                let _ = worker.join();
                return Err(Error::Sip(error));
            }
            Err(error) => {
                let _ = worker.join();
                return Err(Error::Sip(error.to_string()));
            }
        }
        Ok((
            Self(Arc::new(ServiceOwner {
                commands,
                worker: Mutex::new(Some(worker)),
            })),
            inbox,
        ))
    }
    pub fn configure(&self, config: Option<ReceiveConfig>) -> Result<()> {
        if let Some(config) = &config {
            crate::validate_audio_playout_delay(config.account.audio_playout_delay_ms)?;
        }
        self.command(Command::Configure(config, None))
    }
    pub fn configure_listener(
        &self,
        config: ReceiveConfig,
        listener: ListenerConfig,
    ) -> Result<()> {
        listener.validate()?;
        crate::validate_audio_playout_delay(config.account.audio_playout_delay_ms)?;
        self.command(Command::Configure(Some(config), Some(listener)))
    }
    pub fn reconnect(&self) -> Result<()> {
        self.command(Command::Reconnect)
    }
    pub fn cancel_incoming(&self, id: Uuid) -> Result<()> {
        self.command(Command::CancelIncoming(id))
    }
    pub fn submit(
        &self,
        fax: ServiceSend,
        cancelled: Arc<AtomicBool>,
    ) -> Result<mpsc::Receiver<SendUpdate>> {
        crate::validate_audio_playout_delay(fax.account.audio_playout_delay_ms)?;
        let (updates, receiver) = mpsc::unbounded();
        self.command(Command::Send(fax, cancelled, updates))?;
        Ok(receiver)
    }
    fn command(&self, command: Command) -> Result<()> {
        self.0
            .commands
            .send(command)
            .map_err(|_| Error::Sip("SIP service stopped".into()))
    }
    pub fn wake(&self) {
        self.0.commands.wake.notify();
    }
    pub fn shutdown(&self) {
        let _ = self.command(Command::Stop);
        if let Ok(mut worker) = self.0.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}
impl Drop for ServiceOwner {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

struct PreparedConnection {
    route_peer: std::net::SocketAddr,
    local_ip: std::net::IpAddr,
    stun: Option<std::net::SocketAddr>,
    tls: Option<crate::tls::Prepared>,
}
struct Connector {
    result: mpsc::Receiver<Result<PreparedConnection>>,
    cancelled: Arc<AtomicBool>,
    account: ServiceAccount,
}
impl Drop for Connector {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
fn ensure_connection(
    account: &ServiceAccount,
    connections: &mut HashMap<Uuid, Connection>,
    connectors: &mut HashMap<Uuid, Connector>,
    wake: &wake::Wake,
    workers: &mut Vec<thread::JoinHandle<()>>,
) -> Result<()> {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let _ = workers.swap_remove(index).join();
        } else {
            index += 1;
        }
    }
    if connections.contains_key(&account.id) {
        return Ok(());
    }
    if connectors
        .get(&account.id)
        .is_some_and(|connector| connector.account != *account)
    {
        connectors.remove(&account.id);
    }
    if !connectors.contains_key(&account.id) && workers.len() >= 2 {
        return Ok(());
    }
    if let std::collections::hash_map::Entry::Vacant(entry) = connectors.entry(account.id) {
        let (send, result) = mpsc::unbounded();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let owned = account.clone();
        #[cfg(test)]
        let trust = crate::tls::tests::TRUST.with(|trust| trust.borrow().clone());
        let wake = wake.clone();
        let worker = thread::Builder::new()
            .name("faxe-connect".into())
            .spawn(move || {
                #[cfg(test)]
                crate::tls::tests::TRUST.with(|slot| *slot.borrow_mut() = trust);
                let result = (|| -> Result<PreparedConnection> {
                    let peer = (owned.server.as_str(), owned.port)
                        .to_socket_addrs()?
                        .find(|a| a.is_ipv4())
                        .ok_or_else(|| Error::Sip("No IPv4 SIP server address".into()))?;
                    let route = UdpSocket::bind("0.0.0.0:0")?;
                    route.connect(peer)?;
                    let local_ip = route.local_addr()?.ip();
                    let stun = if owned.automatic_nat {
                        owned
                            .stun_server
                            .as_deref()
                            .map(crate::nat::stun_server)
                            .transpose()?
                    } else {
                        None
                    };
                    if stop.load(Ordering::Acquire) {
                        return Err(Error::Cancelled);
                    }
                    let tls = if owned.transport == SignalingTransport::Tls {
                        let (host, port) = tls_peer(&owned.borrowed())?;
                        Some(crate::tls::Transport::prepare(&host, port, &|| {
                            stop.load(Ordering::Acquire)
                        })?)
                    } else {
                        None
                    };
                    Ok(PreparedConnection {
                        route_peer: peer,
                        local_ip,
                        stun,
                        tls,
                    })
                })();
                let _ = send.send(result);
                wake.notify();
            })?;
        workers.push(worker);
        entry.insert(Connector {
            result,
            cancelled,
            account: account.clone(),
        });
    }
    let result = connectors[&account.id].result.try_recv();
    match result {
        Ok(result) => {
            connectors.remove(&account.id);
            connections.insert(account.id, Connection::new(account.clone(), result?)?);
        }
        Err(mpsc::TryRecvError::Empty) => (),
        Err(_) => {
            connectors.remove(&account.id);
            return Err(Error::Sip("Connection setup stopped".into()));
        }
    }
    Ok(())
}
struct Connection {
    listener: Option<ListenerConfig>,
    route_peer: std::net::SocketAddr,
    route_check: Instant,
    local_ip: std::net::IpAddr,
    stun: Option<std::net::SocketAddr>,
    sip: Sip,
    account: ServiceAccount,
    ready: bool,
    deadline: Instant,
    keepalive: Instant,
    registration_peer: Option<pj::pj_sockaddr>,
}
impl Connection {
    fn listening(account: ServiceAccount, listener: ListenerConfig) -> Result<Self> {
        listener.validate()?;
        let ip = listener.advertised_ip.unwrap_or(listener.bind.ip());
        let local = LocalMedia {
            ip,
            audio_port: 0,
            fax_port: 0,
            session: 1,
            version: 1,
        };
        let sip = Sip::new_bound(
            &account.borrowed(),
            local,
            Mode::Auto,
            &|| false,
            None,
            Some(listener.bind),
        )?;
        Ok(Self {
            route_peer: listener.bind,
            local_ip: ip,
            listener: Some(listener),
            route_check: Instant::now(),
            stun: None,
            sip,
            account,
            ready: true,
            deadline: Instant::now(),
            keepalive: Instant::now(),
            registration_peer: None,
        })
    }
    fn new(account: ServiceAccount, prepared: PreparedConnection) -> Result<Self> {
        let local = LocalMedia {
            ip: prepared.local_ip,
            audio_port: 0,
            fax_port: 0,
            session: Uuid::new_v4().as_u128() as u64 & 0x7fff_ffff,
            version: 1,
        };
        let mut sip = Sip::new_prepared(
            &account.borrowed(),
            local,
            Mode::Auto,
            &|| false,
            prepared.tls,
        )?;
        if account.register {
            sip.start_registration(&account.borrowed())?;
        }
        Ok(Self {
            listener: None,
            route_peer: prepared.route_peer,
            route_check: Instant::now(),
            local_ip: prepared.local_ip,
            stun: prepared.stun,
            ready: !account.register,
            sip,
            account,
            deadline: Instant::now() + Duration::from_secs(25),
            keepalive: Instant::now(),
            registration_peer: None,
        })
    }
    fn next_deadline(&self) -> Instant {
        let mut next = if self.listener.is_some() {
            Instant::now() + Duration::from_secs(86400)
        } else {
            self.route_check + Duration::from_secs(5)
        };
        if !self.ready {
            next = next.min(self.deadline);
        }
        if self.account.automatic_nat
            && self.account.transport == SignalingTransport::Udp
            && self.registration_peer.is_some()
        {
            next = next.min(self.keepalive + Duration::from_secs(15));
        }
        if let Some(tls) = &self.sip.tls {
            next = next.min(tls.next_deadline());
        }
        next
    }
    fn poll(&mut self) -> Result<()> {
        if self.listener.is_none() && self.route_check.elapsed() >= Duration::from_secs(5) {
            let socket = UdpSocket::bind("0.0.0.0:0")?;
            socket.connect(self.route_peer)?;
            if socket.local_addr()?.ip() != self.local_ip {
                return Err(Error::Sip("Local network address changed".into()));
            }
            self.route_check = Instant::now();
        }
        if let Some(tls) = &self.sip.tls {
            tls.poll()?;
        }
        SELECTED.set(self.sip.id);
        for event in drain_events() {
            if let Event::Flow(flow, observed, peer) = event {
                unsafe {
                    self.sip.selector.type_ = pj::pjsip_tpselector_type_PJSIP_TPSELECTOR_TRANSPORT;
                    self.sip.selector.u.transport = flow.0;
                    check(pj::pjsip_regc_set_transport(
                        self.sip.registration,
                        &self.sip.selector,
                    ))?;
                    self.sip.flow = Some(flow);
                    self.registration_peer = Some(peer);
                    if self.account.automatic_nat
                        && self.account.transport == SignalingTransport::Udp
                        && let Some((host, port)) = observed
                    {
                        let contact = format!(
                            "<sip:{}@{host}:{port};transport=udp>",
                            self.account.username
                        );
                        if contact != self.sip.contact {
                            self.sip.contact = contact;
                            check(pj::pjsip_regc_update_contact(
                                self.sip.registration,
                                1,
                                &pjstr(&self.sip.contact),
                            ))?;
                            let mut data = ptr::null_mut();
                            check(pj::pjsip_regc_register(self.sip.registration, 1, &mut data))?;
                            check(pj::pjsip_regc_send(self.sip.registration, data))?;
                        }
                    }
                }
            } else if let Event::Registered(code, reason) = event {
                if !(200..300).contains(&code) {
                    return Err(Error::Sip(format!("Registration failed: {code} {reason}")));
                }
                self.ready = true;
                self.sip.registered = true;
            }
        }
        if let Some(flow) = &self.sip.flow {
            unsafe {
                if (*flow.0).is_shutdown != 0 || (*flow.0).is_destroying != 0 {
                    return Err(Error::Sip("Registered SIP flow closed".into()));
                }
                if self.account.automatic_nat
                    && self.account.transport == SignalingTransport::Udp
                    && self.keepalive.elapsed() >= Duration::from_secs(15)
                    && let Some(peer) = &self.registration_peer
                {
                    let status = pj::pjsip_endpt_send_raw(
                        self.sip.endpt,
                        pj::pjsip_transport_type_e_PJSIP_TRANSPORT_UDP,
                        &self.sip.selector,
                        c"\r\n".as_ptr().cast(),
                        2,
                        ptr::from_ref(peer).cast(),
                        pj::pj_sockaddr_get_len(ptr::from_ref(peer).cast()) as i32,
                        ptr::null_mut(),
                        None,
                    );
                    // PJ_EPENDING means the endpoint owns the queued send.
                    if status != 70002 {
                        check(status)?;
                    }
                    self.keepalive = Instant::now();
                }
            }
        }
        if !self.ready && Instant::now() > self.deadline {
            return Err(Error::Sip("Registration timed out".into()));
        }
        Ok(())
    }
}
fn media(
    account: &ServiceAccount,
    ip: std::net::IpAddr,
    stun: Option<std::net::SocketAddr>,
) -> Result<(Sockets, LocalMedia)> {
    let mut sockets = Sockets::bind(ip)?;
    sockets.configure_nat(account.automatic_nat, stun)?;
    let local = LocalMedia {
        ip,
        audio_port: sockets.audio.local_addr()?.port(),
        fax_port: sockets.fax.local_addr()?.port(),
        session: Uuid::new_v4().as_u128() as u64 & 0x7fff_ffff,
        version: 1,
    };
    Ok((sockets, local))
}
impl Sip {
    fn child(&self, local: LocalMedia, mode: Mode) -> Result<Self> {
        let id = NEXT_ID.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        let pool =
            unsafe { pj::pjsip_endpt_create_pool(self.endpt, c"faxe-call".as_ptr(), 4096, 4096) };
        if pool.is_null() {
            return Err(Error::Sip("Could not allocate call pool".into()));
        }
        CALLBACKS.with(|slot| {
            slot.borrow_mut().insert(
                id,
                Callbacks {
                    local,
                    mode,
                    switch_allowed: mode == Mode::Auto,
                    current: if mode == Mode::T38 {
                        Kind::T38
                    } else {
                        Kind::Audio
                    },
                    events: VecDeque::new(),
                },
            )
        });
        Ok(Self {
            network: self.network.clone(),
            flow: self.flow.clone(),
            endpoint: self.endpoint.clone(),
            endpt: self.endpt,
            id,
            pool,
            inv: ptr::null_mut(),
            registration: ptr::null_mut(),
            selector: self.selector,
            contact: self.contact.clone(),
            identity: self.identity.clone(),
            registered: false,
            tls: self.tls.clone(),
        })
    }
}
struct ActiveCall {
    sip: Sip,
    sockets: Sockets,
    request: ServiceSend,
    pump: CallPump,
    cancelled: Arc<AtomicBool>,
    reply: Option<mpsc::Sender<SendUpdate>>,
    incoming: Option<Uuid>,
}
impl ActiveCall {
    fn tick(&mut self, events: &mpsc::Sender<ReceiveEvent>) -> Result<Option<TransferStats>> {
        let request = SendRequest {
            account: self.request.account.borrowed(),
            destination: &self.request.destination,
            file: &self.request.file,
            station_id: &self.request.station_id,
            mode: self.request.mode,
        };
        let id = self.incoming;
        let reply = &self.reply;
        self.pump.step(
            &mut self.sip,
            &self.sockets,
            &request,
            &|| self.cancelled.load(Ordering::Acquire),
            &mut |event| {
                if let Some(id) = id {
                    let _ = events.send(ReceiveEvent::Progress { id, event });
                } else if let Some(reply) = reply {
                    let _ = reply.send(SendUpdate::Progress(event));
                }
            },
        )
    }
    fn finish(
        mut self,
        outcome: Result<TransferStats>,
        events: &mpsc::Sender<ReceiveEvent>,
    ) -> Sip {
        let transport = self.pump.remote.as_ref().map(|remote| {
            if remote.kind() == Kind::T38 {
                Mode::T38
            } else {
                Mode::G711
            }
        });
        // Callbacks remain alive until explicit finalization returns. Drop native
        // state before notifying the exporter, including on cancellation/failure.
        if let Err(error) = self.pump.drain_audio() {
            tracing::warn!(%error, "Could not drain buffered fax audio at call end");
        }
        let recovery = self.incoming.and_then(|_| {
            self.pump.fax.as_mut().map(|fax| match fax {
                FaxMedia::Audio(modem) => modem.finalize_receive(),
                FaxMedia::Packets(modem) => modem.finalize_receive(),
            })
        });
        self.pump.fax.take();
        self.sip.end_call();
        if let Some(id) = self.incoming {
            let _ = events.send(ReceiveEvent::Finished {
                id,
                outcome,
                transport,
                recovery,
            });
        } else if let Some(reply) = self.reply {
            let _ = reply.send(SendUpdate::Finished(outcome));
        }
        self.sip
    }
}
struct IncomingSlot {
    direct: bool,
    admission_timeout: Duration,
    sip: Sip,
    sockets: Sockets,
    config: ReceiveConfig,
}
thread_local! {
    static COMMANDS: RefCell<Option<Commands>> = const { RefCell::new(None) };
    static AVAILABLE: RefCell<VecDeque<IncomingSlot>> = const { RefCell::new(VecDeque::new()) };
    static ADMISSION: RefCell<HashMap<Uuid, PendingAdmission>> = RefCell::new(HashMap::new());
    static INCOMING_CALL_IDS: RefCell<HashMap<Uuid, String>> = RefCell::new(HashMap::new());
    static INCOMING_LIMIT: Cell<usize> = const { Cell::new(1) };
}
fn forget_incoming(id: Uuid) {
    INCOMING_CALL_IDS.with(|ids| ids.borrow_mut().remove(&id));
}

pub(super) unsafe fn on_incoming(data: *mut pj::pjsip_rx_data) -> pj::pj_bool_t {
    unsafe {
        let call_id = string((*(*data).msg_info.cid).id);
        if INCOMING_CALL_IDS.with(|ids| ids.borrow().values().any(|id| id == &call_id)) {
            return 0;
        }
        let endpoint = (*(*data).tp_info.transport).endpt;
        let respond = |code| {
            pj::pjsip_endpt_respond_stateless(
                endpoint,
                data,
                code,
                ptr::null(),
                ptr::null(),
                ptr::null(),
            );
        };
        if INCOMING_CALL_IDS.with(|ids| ids.borrow().len()) >= INCOMING_LIMIT.get() {
            respond(486);
            return 1;
        }
        let Some(mut slot) = AVAILABLE.with(|slot| slot.borrow_mut().pop_front()) else {
            respond(480);
            return 1;
        };
        // Request URI is printed through PJSIP to avoid assumptions about URI layout.
        let mut uri = [0_u8; 2048];
        let request_uri = (*(*data).msg_info.msg).line.req.uri;
        let length = ((*(*request_uri).vptr).p_print.unwrap())(
            pj::pjsip_uri_context_e_PJSIP_URI_IN_REQ_URI,
            (*(*data).msg_info.msg).line.req.uri.cast(),
            uri.as_mut_ptr().cast(),
            uri.len(),
        );
        let target = if length > 0 {
            String::from_utf8_lossy(&uri[..length as usize]).into_owned()
        } else {
            String::new()
        };
        let user = target
            .split_once(':')
            .and_then(|(_, value)| value.split_once('@'))
            .map(|(user, _)| user);
        if !slot.direct && user != Some(slot.config.account.username.as_str()) {
            AVAILABLE.with(|available| available.borrow_mut().push_front(slot));
            respond(404);
            return 1;
        }
        if slot.sockets.mapping_pending() {
            AVAILABLE.with(|available| available.borrow_mut().push_front(slot));
            respond(480);
            return 1;
        }
        let admission_timeout = slot.admission_timeout;
        SELECTED.set(slot.sip.id);
        let result = (|| -> Result<(ActiveCall, AdmissionRequest, AdmissionCallback)> {
            let info = pj::pjsip_rdata_get_sdp_info(data);
            if !info.is_null() {
                check((*info).sdp_err)?;
            }
            let offer = if info.is_null() || (*info).sdp.is_null() {
                None
            } else {
                Some(print_sdp((*info).sdp)?)
            };
            let local_sdp = CALLBACKS.with(|callbacks| {
                let map = callbacks.borrow();
                let local = &map.get(&slot.sip.id).unwrap().local;
                match offer {
                    Some(ref offer) => local.answer(
                        offer,
                        slot.config.mode != Mode::T38,
                        slot.config.mode != Mode::G711,
                    ),
                    None => Ok(local.offer(if slot.config.mode == Mode::T38 {
                        Kind::T38
                    } else {
                        Kind::Audio
                    })),
                }
            })?;
            let answer = parse_sdp(slot.sip.pool, &local_sdp)?;
            let mut options = 0;
            check(pj::pjsip_inv_verify_request(
                data,
                &mut options,
                answer,
                ptr::null_mut(),
                endpoint,
                ptr::null_mut(),
            ))?;
            let mut dialog = ptr::null_mut();
            check(pj::pjsip_dlg_create_uas_and_inc_lock(
                pj::pjsip_ua_instance(),
                data,
                &pjstr(&slot.sip.contact),
                &mut dialog,
            ))?;
            let setup = (|| -> Result<()> {
                check(pj::pjsip_dlg_set_transport(dialog, &slot.sip.selector))?;
                if let Some(credential) = credential(&slot.config.account.borrowed()) {
                    check(pj::pjsip_auth_clt_set_credentials(
                        &mut (*dialog).auth_sess,
                        1,
                        &credential,
                    ))?;
                }
                check(pj::pjsip_inv_create_uas(
                    dialog,
                    data,
                    answer,
                    options,
                    &mut slot.sip.inv,
                ))?;
                check(pj::pjsip_inv_add_ref(slot.sip.inv))?;
                INVITES.with(|map| map.borrow_mut().insert(slot.sip.inv as usize, slot.sip.id));
                Ok(())
            })();
            pj::pjsip_dlg_dec_lock(dialog);
            setup?;
            let id = Uuid::new_v4();
            let mut caller = [0_u8; 2048];
            let length = pj::pjsip_hdr_print_on(
                (*data).msg_info.from.cast(),
                caller.as_mut_ptr().cast(),
                caller.len(),
            );
            let caller = if length > 0 {
                String::from_utf8_lossy(&caller[..length as usize])
                    .trim()
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect()
            } else {
                "Unknown caller".into()
            };
            let pending = Arc::new(AtomicBool::new(true));
            let commands = COMMANDS
                .with(|slot| slot.borrow().clone())
                .ok_or_else(|| Error::Sip("SIP service is stopping".into()))?;
            let peer =
                std::ffi::CStr::from_ptr((*data).pkt_info.src_name.as_ptr()).to_string_lossy();
            let peer = format!("{}:{}", peer, (*data).pkt_info.src_port);
            let admission = AdmissionRequest {
                id,
                caller,
                destination: target,
                peer,
                arrived: SystemTime::now(),
                pending,
                commands,
            };
            let offer = slot.config.offer;
            let file = PathBuf::new();
            let mut response = ptr::null_mut();
            let answered = check(pj::pjsip_inv_initial_answer(
                slot.sip.inv,
                data,
                100,
                ptr::null(),
                ptr::null(),
                &mut response,
            ))
            .and_then(|_| check(pj::pjsip_inv_send_msg(slot.sip.inv, response)));
            if let Err(error) = answered {
                push(Event::MediaError(error.to_string()));
            }
            Ok((
                ActiveCall {
                    sip: slot.sip,
                    sockets: slot.sockets,
                    request: ServiceSend {
                        account: slot.config.account,
                        destination: String::new(),
                        file,
                        station_id: slot.config.station_id,
                        mode: slot.config.mode,
                    },
                    pump: CallPump::new(true, slot.config.ecm),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    reply: None,
                    incoming: Some(id),
                },
                admission,
                offer,
            ))
        })();
        match result {
            Ok((call, request, offer)) => {
                INCOMING_CALL_IDS.with(|ids| ids.borrow_mut().insert(request.id, call_id));
                ADMISSION.with(|slot| {
                    slot.borrow_mut().insert(
                        request.id,
                        PendingAdmission {
                            call,
                            deadline: Instant::now() + admission_timeout,
                            valid: request.pending.clone(),
                        },
                    )
                });
                offer(request);
            }
            Err(error) => {
                tracing::warn!(%error, "Incoming fax rejected");
                respond(488);
            }
        }
        1
    }
}

fn reject_admission(sip: &mut Sip, code: i32) {
    unsafe {
        if !sip.inv.is_null()
            && (*sip.inv).state != pj::pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED
        {
            let mut response = ptr::null_mut();
            if pj::pjsip_inv_answer(sip.inv, code, ptr::null(), ptr::null(), &mut response) == 0 {
                pj::pjsip_inv_send_msg(sip.inv, response);
            }
        }
    }
}

struct PendingSend {
    fax: ServiceSend,
    cancelled: Arc<AtomicBool>,
    reply: mpsc::Sender<SendUpdate>,
    media: Option<(Sockets, LocalMedia)>,
}
impl PendingSend {
    fn prepare(
        &mut self,
        connections: &mut HashMap<Uuid, Connection>,
        connectors: &mut HashMap<Uuid, Connector>,
        wake: &wake::Wake,
        workers: &mut Vec<thread::JoinHandle<()>>,
    ) -> Result<Option<ActiveCall>> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        ensure_connection(&self.fax.account, connections, connectors, wake, workers)?;
        let Some(connection) = connections.get(&self.fax.account.id) else {
            return Ok(None);
        };
        if connection.account != self.fax.account {
            return Err(Error::Sip(
                "Queued profile differs from the active account; requeue with the current profile"
                    .into(),
            ));
        }
        if !connection.ready {
            return Ok(None);
        }
        if self.media.is_none() {
            self.media = Some(media(
                &self.fax.account,
                connection.local_ip,
                connection.stun,
            )?);
        }
        let (sockets, _) = self.media.as_ref().unwrap();
        sockets.poll_mappings()?;
        if sockets.mapping_pending() {
            return Ok(None);
        }
        let (sockets, local) = self.media.take().unwrap();
        let mut sip = connection
            .sip
            .child(sockets.mapped_local(local), self.fax.mode)?;
        sip.invite(
            &SendRequest {
                account: self.fax.account.borrowed(),
                destination: &self.fax.destination,
                file: &self.fax.file,
                station_id: &self.fax.station_id,
                mode: self.fax.mode,
            },
            if self.fax.mode == Mode::T38 {
                Kind::T38
            } else {
                Kind::Audio
            },
        )?;
        Ok(Some(ActiveCall {
            sip,
            sockets,
            request: ServiceSend {
                account: self.fax.account.clone(),
                destination: self.fax.destination.clone(),
                file: self.fax.file.clone(),
                station_id: self.fax.station_id.clone(),
                mode: self.fax.mode,
            },
            pump: CallPump::new(false, true),
            cancelled: self.cancelled.clone(),
            reply: Some(self.reply.clone()),
            incoming: None,
        }))
    }
    fn fail(self, error: Error) {
        let _ = self.reply.send(SendUpdate::Finished(Err(error)));
    }
}

fn service_loop(
    commands: mpsc::Receiver<Command>,
    events: &mpsc::Sender<ReceiveEvent>,
    command_sender: Commands,
    wake_reader: UdpSocket,
    options: ServiceOptions,
    ready: mpsc::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let endpoint = Endpoint::get()?;
    let _wake = wake::Registration::new(endpoint.clone(), wake_reader)?;
    let wake = command_sender.wake.clone();
    let mut connector_workers = Vec::new();
    COMMANDS.with(|slot| *slot.borrow_mut() = Some(command_sender));
    INCOMING_LIMIT.set(options.limits.incoming);
    let _ = ready.send(Ok(()));
    let mut connections: HashMap<Uuid, Connection> = HashMap::new();
    let mut connectors = HashMap::new();
    let mut desired: Option<ReceiveConfig> = None;
    let mut listener: Option<ListenerConfig> = None;
    let mut calls: HashMap<Uuid, ActiveCall> = HashMap::new();
    let mut pending: VecDeque<PendingSend> = VecDeque::new();
    let mut retired: Vec<(Instant, Sip)> = Vec::new();
    let mut retry_at = Instant::now();
    let mut backoff = 1_u64;
    let mut last_status = None;
    let mut stopping = false;
    let mut last_tick = Instant::now();
    let mut reconnect_requested = false;
    let mut next_command = None;
    let outcome = (|| -> Result<()> {
        loop {
            let woke = last_tick.elapsed() > Duration::from_secs(30);
            if woke {
                for (id, call) in calls.drain() {
                    forget_incoming(id);
                    retired.push((
                        Instant::now(),
                        call.finish(
                            Err(Error::Sip(
                                "Transfer interrupted while the computer was asleep".into(),
                            )),
                            events,
                        ),
                    ));
                }
                ADMISSION.with(|admissions| {
                    for (id, mut admission) in admissions.borrow_mut().drain() {
                        admission.valid.store(false, Ordering::Release);
                        reject_admission(&mut admission.call.sip, 480);
                        forget_incoming(id);
                        retired.push((Instant::now(), admission.call.sip));
                    }
                });
            }
            reconnect_requested |= woke;
            last_tick = Instant::now();
            // A command burst must leave time for media and native socket polling.
            for command in next_command
                .take()
                .into_iter()
                .chain(commands.try_iter())
                .take(64)
            {
                match command {
                    Command::Configure(config, direct) => {
                        desired = config;
                        listener = direct;
                        AVAILABLE.with(|slots| slots.borrow_mut().clear());
                        retry_at = Instant::now();
                        backoff = 1;
                    }
                    Command::CancelIncoming(id) => {
                        if let Some(call) = calls.get(&id) {
                            call.cancelled.store(true, Ordering::Release);
                        }
                    }
                    Command::Reject(id) => {
                        if let Some(mut admission) = ADMISSION.with(|a| a.borrow_mut().remove(&id))
                        {
                            admission.valid.store(false, Ordering::Release);
                            reject_admission(&mut admission.call.sip, 603);
                            forget_incoming(id);
                            retired.push((Instant::now(), admission.call.sip));
                        }
                    }
                    Command::Admit(id, result) => {
                        let admission = ADMISSION.with(|a| a.borrow_mut().remove(&id));
                        if let Some(mut admission) = admission {
                            admission.valid.store(false, Ordering::Release);
                            let persisted = result.is_ok();
                            let result = result.and_then(|path| {
                                if Instant::now() >= admission.deadline
                                    || unsafe {
                                        (*admission.call.sip.inv).state
                                            == pj::pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED
                                    }
                                {
                                    return Err(Error::Cancelled);
                                }
                                admission.call.request.file = path;
                                let mut response = ptr::null_mut();
                                unsafe {
                                    check(pj::pjsip_inv_answer(
                                        admission.call.sip.inv,
                                        200,
                                        ptr::null(),
                                        ptr::null(),
                                        &mut response,
                                    ))?;
                                    check(pj::pjsip_inv_send_msg(admission.call.sip.inv, response))
                                }
                            });
                            match result {
                                Ok(()) => {
                                    calls.insert(id, admission.call);
                                }
                                Err(error) => {
                                    reject_admission(&mut admission.call.sip, 500);
                                    let sip = if persisted {
                                        admission.call.finish(Err(error), events)
                                    } else {
                                        admission.call.sip
                                    };
                                    forget_incoming(id);
                                    retired.push((Instant::now(), sip));
                                }
                            }
                        } else if result.is_ok() {
                            // Persistence raced a timeout/CANCEL; close the persisted record.
                            let _ = events.send(ReceiveEvent::Finished {
                                id,
                                outcome: Err(Error::Cancelled),
                                transport: None,
                                recovery: None,
                            });
                        }
                    }
                    Command::Reconnect => reconnect_requested = true,
                    Command::Stop => stopping = true,
                    Command::Send(fax, cancelled, reply) => {
                        let count =
                            calls.values().filter(|c| c.incoming.is_none()).count() + pending.len();
                        if count >= options.limits.outgoing {
                            let _ = reply.send(SendUpdate::Finished(Err(Error::Sip(
                                "Outgoing call capacity reached".into(),
                            ))));
                        } else {
                            pending.push_back(PendingSend {
                                fax,
                                cancelled,
                                reply,
                                media: None,
                            });
                        }
                    }
                }
            }
            if stopping {
                break;
            }
            if reconnect_requested && calls.is_empty() && ADMISSION.with(|a| a.borrow().is_empty())
            {
                reconnect_requested = false;
                connectors.clear();
                AVAILABLE.with(|s| s.borrow_mut().clear());
                for (_, mut connection) in connections.drain() {
                    connection.sip.unregister();
                    retired.push((Instant::now(), connection.sip));
                }
                retry_at = Instant::now();
                backoff = 1;
            }
            let failures: Vec<_> = connections
                .iter_mut()
                .filter_map(|(&id, c)| c.poll().err().map(|e| (id, e.to_string())))
                .collect();
            for (account_id, error) in failures {
                let mut remaining = VecDeque::new();
                for send in pending.drain(..) {
                    if send.fax.account.id == account_id {
                        send.fail(Error::Sip(error.clone()));
                    } else {
                        remaining.push_back(send);
                    }
                }
                pending = remaining;
                let affected: Vec<_> = calls
                    .iter()
                    .filter(|(_, c)| c.request.account.id == account_id)
                    .map(|(&id, _)| id)
                    .collect();
                for id in affected {
                    forget_incoming(id);
                    retired.push((
                        Instant::now(),
                        calls
                            .remove(&id)
                            .unwrap()
                            .finish(Err(Error::Sip(error.clone())), events),
                    ));
                }
                ADMISSION.with(|a| {
                    let ids: Vec<_> = a
                        .borrow()
                        .iter()
                        .filter(|(_, p)| p.call.request.account.id == account_id)
                        .map(|(&id, _)| id)
                        .collect();
                    for id in ids {
                        let mut p = a.borrow_mut().remove(&id).unwrap();
                        p.valid.store(false, Ordering::Release);
                        reject_admission(&mut p.call.sip, 480);
                        forget_incoming(id);
                        retired.push((Instant::now(), p.call.sip));
                    }
                });
                if let Some(connection) = connections.remove(&account_id) {
                    retired.push((Instant::now(), connection.sip));
                }
                if desired.as_ref().is_some_and(|c| c.account.id == account_id) {
                    AVAILABLE.with(|s| s.borrow_mut().clear());
                    retry_at = Instant::now()
                        + Duration::from_secs(
                            if error.contains("401")
                                || error.contains("403")
                                || error.contains("certificate")
                            {
                                86400
                            } else {
                                backoff
                            },
                        );
                    backoff = (backoff * 2).min(60);
                    last_status = Some((
                        if listener.is_some() {
                            None
                        } else {
                            Some(account_id)
                        },
                        ReceiveState::Error(error),
                    ));
                    let (profile, state) = last_status.clone().unwrap();
                    let _ = events.send(ReceiveEvent::Status { profile, state });
                }
            }
            if let Some(config) = &desired
                && Instant::now() >= retry_at
                && !calls
                    .values()
                    .any(|c| c.incoming.is_some() && c.request.account.id != config.account.id)
                && !ADMISSION.with(|a| {
                    a.borrow()
                        .values()
                        .any(|p| p.call.request.account.id != config.account.id)
                })
            {
                let held = calls
                    .values()
                    .any(|c| c.request.account.id == config.account.id)
                    || ADMISSION.with(|a| {
                        a.borrow()
                            .values()
                            .any(|p| p.call.request.account.id == config.account.id)
                    });
                let changed = connections
                    .get(&config.account.id)
                    .is_some_and(|c| c.account != config.account || c.listener != listener);
                if changed && !held {
                    AVAILABLE.with(|s| s.borrow_mut().clear());
                    if let Some(mut c) = connections.remove(&config.account.id) {
                        c.sip.unregister();
                        retired.push((Instant::now(), c.sip));
                    }
                }
                let connection_result = if changed && held {
                    Ok(())
                } else if let Some(direct) = &listener {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        connections.entry(config.account.id)
                    {
                        Connection::listening(config.account.clone(), direct.clone()).map(|c| {
                            entry.insert(c);
                        })
                    } else {
                        Ok(())
                    }
                } else {
                    ensure_connection(
                        &config.account,
                        &mut connections,
                        &mut connectors,
                        &wake,
                        &mut connector_workers,
                    )
                };
                if let Err(error) = connection_result {
                    let error = error.to_string();
                    retry_at = Instant::now()
                        + Duration::from_secs(if error.contains("certificate") {
                            86400
                        } else {
                            backoff
                        });
                    backoff = (backoff * 2).min(60);
                    let profile = if listener.is_some() {
                        None
                    } else {
                        Some(config.account.id)
                    };
                    last_status = Some((profile, ReceiveState::Error(error.clone())));
                    let _ = events.send(ReceiveEvent::Status {
                        profile,
                        state: ReceiveState::Error(error),
                    });
                }
                if let Some(connection) = connections.get(&config.account.id)
                    && connection.ready
                    && connection.account == config.account
                    && connection.listener == listener
                {
                    let occupied = INCOMING_CALL_IDS.with(|ids| ids.borrow().len());
                    let available = AVAILABLE.with(|s| s.borrow().len());
                    for _ in occupied + available..options.limits.incoming {
                        let bind_ip = listener
                            .as_ref()
                            .map(|l| l.bind.ip())
                            .unwrap_or(connection.local_ip);
                        let (sockets, mut local) =
                            media(&config.account, bind_ip, connection.stun)?;
                        local.ip = connection.local_ip;
                        let sip = connection.sip.child(local, config.mode)?;
                        AVAILABLE.with(|s| {
                            s.borrow_mut().push_back(IncomingSlot {
                                direct: listener.is_some(),
                                admission_timeout: options.admission_timeout,
                                sip,
                                sockets,
                                config: config.clone(),
                            })
                        });
                    }
                    backoff = 1;
                }
            }
            let mut waiting = VecDeque::new();
            for mut send in pending.drain(..) {
                match send.prepare(
                    &mut connections,
                    &mut connectors,
                    &wake,
                    &mut connector_workers,
                ) {
                    Ok(Some(call)) => {
                        calls.insert(Uuid::new_v4(), call);
                    }
                    Ok(None) => waiting.push_back(send),
                    Err(error) => send.fail(error),
                }
            }
            pending = waiting;
            AVAILABLE.with(|slots| -> Result<()> {
                for slot in slots.borrow().iter() {
                    slot.sockets.poll_mappings()?;
                    CALLBACKS.with(|callbacks| {
                        if let Some(c) = callbacks.borrow_mut().get_mut(&slot.sip.id) {
                            c.local = slot.sockets.mapped_local(c.local.clone());
                        }
                    });
                }
                Ok(())
            })?;
            ADMISSION.with(|a| {
                let expired: Vec<_> = a
                    .borrow()
                    .iter()
                    .filter(|(_, p)| {
                        Instant::now() >= p.deadline
                            || unsafe {
                                (*p.call.sip.inv).state
                                    == pj::pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED
                            }
                    })
                    .map(|(&id, _)| id)
                    .collect();
                for id in expired {
                    let mut p = a.borrow_mut().remove(&id).unwrap();
                    p.valid.store(false, Ordering::Release);
                    reject_admission(&mut p.call.sip, 480);
                    forget_incoming(id);
                    retired.push((Instant::now(), p.call.sip));
                }
            });
            let mut finished = Vec::new();
            for (&id, call) in &mut calls {
                match call.tick(events) {
                    Ok(None) => (),
                    result => finished.push((id, result.map(|s| s.unwrap()))),
                }
            }
            for (id, result) in finished {
                forget_incoming(id);
                retired.push((
                    Instant::now(),
                    calls.remove(&id).unwrap().finish(result, events),
                ));
            }
            let profile = desired.as_ref().map(|c| c.account.id);
            let active_profile = calls
                .values()
                .find(|c| c.incoming.is_some())
                .map(|c| c.request.account.id)
                .or_else(|| {
                    ADMISSION.with(|a| {
                        a.borrow()
                            .values()
                            .next()
                            .map(|p| p.call.request.account.id)
                    })
                });
            let status_profile = active_profile.or(profile).filter(|id| !id.is_nil());
            let receiving = INCOMING_CALL_IDS.with(|ids| !ids.borrow().is_empty());
            let available =
                AVAILABLE.with(|s| s.borrow().iter().any(|s| !s.sockets.mapping_pending()));
            let state = if receiving {
                ReceiveState::Receiving
            } else if desired.is_none() {
                ReceiveState::Off
            } else if Instant::now() < retry_at {
                last_status
                    .as_ref()
                    .map(|(_, s)| s.clone())
                    .unwrap_or(ReceiveState::Registering)
            } else if available {
                ReceiveState::Ready
            } else {
                ReceiveState::Registering
            };
            if last_status.as_ref() != Some(&(status_profile, state.clone())) {
                let _ = events.send(ReceiveEvent::Status {
                    profile: status_profile,
                    state: state.clone(),
                });
                last_status = Some((status_profile, state));
            }
            connectors.retain(|id, _| {
                Some(*id) == profile || pending.iter().any(|p| p.fax.account.id == *id)
            });
            let unused: Vec<_> = connections
                .keys()
                .copied()
                .filter(|id| {
                    Some(*id) != profile
                        && !calls.values().any(|c| c.request.account.id == *id)
                        && !pending.iter().any(|p| p.fax.account.id == *id)
                        && !ADMISSION.with(|a| {
                            a.borrow()
                                .values()
                                .any(|p| p.call.request.account.id == *id)
                        })
                })
                .collect();
            for id in unused {
                if let Some(mut c) = connections.remove(&id) {
                    c.sip.unregister();
                    retired.push((Instant::now(), c.sip));
                }
            }
            for (_, sip) in &retired {
                if let Some(tls) = &sip.tls {
                    let _ = tls.poll();
                }
            }
            retired.retain(|(at, _)| at.elapsed() < Duration::from_secs(2));
            endpoint.tls_owners.borrow_mut().retain(|t| !t.destroyed());
            if desired.is_none()
                && pending.is_empty()
                && calls.is_empty()
                && connections.is_empty()
                && connectors.is_empty()
                && retired.is_empty()
                && ADMISSION.with(|a| a.borrow().is_empty())
            {
                match commands.recv() {
                    Ok(c) => next_command = Some(c),
                    Err(_) => break,
                }
                last_tick = Instant::now();
                continue;
            }
            let mut deadline = connections.values().map(Connection::next_deadline).min();
            let mut due = |at: Option<Instant>| {
                if let Some(at) = at {
                    deadline = Some(deadline.map_or(at, |old| old.min(at)));
                }
            };
            for call in calls.values() {
                due(Some(call.pump.next_frame));
                due(call.sockets.next_deadline());
            }
            for send in &pending {
                if let Some((s, _)) = &send.media {
                    due(s.next_deadline());
                }
            }
            AVAILABLE.with(|s| {
                for s in s.borrow().iter() {
                    due(s.sockets.next_deadline());
                }
            });
            ADMISSION.with(|a| {
                for p in a.borrow().values() {
                    due(Some(p.deadline));
                }
            });
            for (at, sip) in &retired {
                due(Some(*at + Duration::from_secs(2)));
                if let Some(tls) = &sip.tls {
                    due(Some(tls.next_deadline()));
                }
            }
            if desired.is_some() && retry_at > Instant::now() {
                due(Some(retry_at));
            }
            // Refill a released incoming slot before sleeping for the next INVITE.
            if let Some(config) = &desired
                && Instant::now() >= retry_at
                && connections.get(&config.account.id).is_some_and(|c| {
                    c.ready && c.account == config.account && c.listener == listener
                })
                && INCOMING_CALL_IDS.with(|i| i.borrow().len())
                    + AVAILABLE.with(|s| s.borrow().len())
                    < options.limits.incoming
            {
                due(Some(Instant::now()));
            }
            if !commands.is_empty() {
                due(Some(Instant::now()));
            }
            let millis = deadline
                .map(|d| {
                    d.saturating_duration_since(Instant::now())
                        .as_millis()
                        .saturating_add(1)
                })
                .unwrap_or(86400000);
            unsafe {
                check(pj::pjsip_endpt_handle_events(
                    endpoint.endpt,
                    &pj::pj_time_val {
                        sec: (millis / 1000) as _,
                        msec: (millis % 1000) as _,
                    },
                ))?;
            }
        }
        Ok(())
    })();
    connectors.clear();
    ADMISSION.with(|a| {
        for (_, mut p) in a.borrow_mut().drain() {
            p.valid.store(false, Ordering::Release);
            reject_admission(&mut p.call.sip, 480);
            retired.push((Instant::now(), p.call.sip));
        }
    });
    AVAILABLE.with(|s| s.borrow_mut().clear());
    for (_, call) in calls {
        retired.push((
            Instant::now(),
            call.finish(
                Err(match &outcome {
                    Ok(()) => Error::Cancelled,
                    Err(error) => Error::Sip(error.to_string()),
                }),
                events,
            ),
        ));
    }
    for send in pending {
        send.fail(Error::Cancelled);
    }
    for command in commands.try_iter() {
        if let Command::Send(_, _, reply) = command {
            let _ = reply.send(SendUpdate::Finished(Err(Error::Cancelled)));
        }
    }
    for (_, mut c) in connections {
        c.sip.unregister();
        retired.push((Instant::now(), c.sip));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !retired.is_empty() && Instant::now() < deadline {
        for (_, sip) in &retired {
            if let Some(tls) = &sip.tls {
                let _ = tls.poll();
            }
        }
        unsafe {
            pj::pjsip_endpt_handle_events(endpoint.endpt, &pj::pj_time_val { sec: 0, msec: 20 });
        }
        retired.retain(|(at, _)| at.elapsed() < Duration::from_secs(2));
    }
    INCOMING_CALL_IDS.with(|ids| ids.borrow_mut().clear());
    COMMANDS.with(|s| s.borrow_mut().take());
    for worker in connector_workers {
        let _ = worker.join();
    }
    outcome
}
