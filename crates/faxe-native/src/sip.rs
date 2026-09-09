pub(crate) mod io;
mod service;
mod wake;
use crate::{
    AudioFax, Error, FaxEvent, FaxStage, PacketFax, Result, TransferStats,
    logging::{self, PjLogging},
    network::{AudioNetwork, PacketNetwork, Sockets},
    sdp::{self, Kind, LocalMedia, RemoteMedia},
};
use faxe_pj_sys as pj;
pub use service::*;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    marker::PhantomData,
    net::{ToSocketAddrs, UdpSocket},
    path::Path,
    ptr,
    rc::Rc,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingTransport {
    Udp,
    Tcp,
    Tls,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    G711,
    T38,
}

pub struct Account<'a> {
    pub server: &'a str,
    pub port: u16,
    pub transport: SignalingTransport,
    pub username: &'a str,
    pub auth_username: Option<&'a str>,
    pub password: Option<&'a str>,
    pub register: bool,
    pub outbound_proxy: Option<&'a str>,
}

pub struct SendRequest<'a> {
    pub account: Account<'a>,
    pub destination: &'a str,
    pub file: &'a Path,
    pub station_id: &'a str,
    pub mode: Mode,
}

static ENDPOINT_OWNER: Mutex<()> = Mutex::new(());

fn tls_peer(account: &Account<'_>) -> Result<(String, u16)> {
    match account.outbound_proxy {
        None => Ok((account.server.into(), account.port)),
        Some(proxy) => {
            let uri = proxy
                .strip_prefix("sips:")
                .or_else(|| proxy.strip_prefix("sip:"))
                .ok_or_else(|| Error::Sip("Invalid TLS proxy URI".into()))?;
            let mut parts = uri.split(';');
            let authority = parts
                .next()
                .unwrap_or_default()
                .rsplit('@')
                .next()
                .unwrap_or_default();
            if parts.any(|part| part.starts_with("transport=") && part != "transport=tls") {
                return Err(Error::Sip(
                    "TLS profiles require a TLS outbound proxy".into(),
                ));
            }
            match authority.rsplit_once(':') {
                Some((host, port)) => Ok((
                    host.into(),
                    port.parse()
                        .map_err(|_| Error::Sip("Invalid TLS proxy port".into()))?,
                )),
                None => Ok((authority.into(), 5061)),
            }
        }
    }
}

fn route_uri(account: &Account<'_>, uri: &str) -> Result<String> {
    match account.transport {
        SignalingTransport::Tls => {
            if uri
                .split(';')
                .any(|part| part.starts_with("transport=") && part != "transport=tls")
            {
                return Err(Error::Sip(
                    "TLS profiles cannot use a plaintext SIP route".into(),
                ));
            }
            match uri.starts_with("sips:") || uri.contains(";transport=tls") {
                true => Ok(uri.to_owned()),
                false => Ok(format!("{uri};transport=tls")),
            }
        }
        _ => Ok(uri.to_owned()),
    }
}

/// Runs a complete call on the current thread. No native handle escapes this function.
pub fn send(
    request: SendRequest<'_>,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(FaxEvent),
) -> Result<TransferStats> {
    progress(FaxEvent::Stage(FaxStage::Connecting));
    let _owner = loop {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        match ENDPOINT_OWNER.try_lock() {
            Ok(owner) => break owner,
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(_) => return Err(Error::Sip("Native endpoint owner is poisoned".into())),
        }
    };
    if request.account.transport != SignalingTransport::Tls
        && (request.destination.starts_with("sips:")
            || request
                .account
                .outbound_proxy
                .is_some_and(|proxy| proxy.starts_with("sips:")))
    {
        return Err(Error::Sip(
            "A sips: destination requires a TLS SIP profile".into(),
        ));
    }
    for value in [
        request.account.server,
        request.account.username,
        request.destination,
    ] {
        if value.is_empty() || value.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(Error::Invalid(
                "Invalid SIP server, username, or destination".into(),
            ));
        }
    }
    let server = (request.account.server, request.account.port)
        .to_socket_addrs()?
        .find(|address| address.is_ipv4())
        .ok_or_else(|| Error::Sip("No IPv4 SIP server address found".into()))?;
    let routing = UdpSocket::bind("0.0.0.0:0")?;
    routing.connect(server)?;
    let ip = routing.local_addr()?.ip();
    let sockets = Sockets::bind(ip)?;
    let local = LocalMedia {
        ip,
        audio_port: sockets.audio.local_addr()?.port(),
        fax_port: sockets.fax.local_addr()?.port(),
        session: uuid::Uuid::new_v4().as_u128() as u64 & 0x7fff_ffff,
        version: 1,
    };
    tracing::debug!(mode = ?request.mode, transport = ?request.account.transport, local_ip = %ip, audio_port = local.audio_port, fax_port = local.fax_port, "Starting fax call");
    if request.account.transport == SignalingTransport::Tls {
        progress(FaxEvent::Stage(FaxStage::Securing));
    }
    let mut sip = Sip::new(&request.account, local, request.mode, &cancelled)?;
    let outcome = (|| {
        if request.account.register {
            progress(FaxEvent::Stage(FaxStage::Registering));
            sip.register(&request.account, &cancelled)?;
        }
        let initial = match request.mode {
            Mode::T38 => Kind::T38,
            _ => Kind::Audio,
        };
        progress(FaxEvent::Stage(FaxStage::Calling));
        sip.invite(&request, initial)?;
        run_call(&mut sip, &sockets, &request, &cancelled, &mut progress)
    })();
    match &outcome {
        Ok(stats) => tracing::info!(?stats, "Fax transmission completed"),
        Err(error) => tracing::error!(%error, "Fax transmission failed"),
    }
    sip.finish();
    outcome
}

enum Attempt {
    NotStarted,
    Waiting {
        cseq: i32,
        since: Instant,
        cancelling: bool,
    },
    Rejected,
}
enum FaxMedia {
    Audio(AudioFax),
    Packets(PacketFax),
}

struct CallPump {
    started: Instant,
    connected: Option<Instant>,
    remote: Option<RemoteMedia>,
    audio: Option<AudioNetwork>,
    packets: Option<PacketNetwork>,
    fax: Option<FaxMedia>,
    attempt: Attempt,
    direct_retry: bool,
    next_frame: Instant,
    cng_samples: usize,
    last_activity_report: Instant,
    reported_bytes: u64,
    receiving: bool,
    ecm: bool,
}
impl CallPump {
    fn new(receiving: bool, ecm: bool) -> Self {
        Self {
            started: Instant::now(),
            connected: None,
            remote: None,
            audio: None,
            packets: None,
            fax: None,
            attempt: Attempt::NotStarted,
            direct_retry: false,
            next_frame: Instant::now(),
            cng_samples: 0,
            last_activity_report: Instant::now(),
            reported_bytes: 0,
            receiving,
            ecm,
        }
    }
    fn step(
        &mut self,
        sip: &mut Sip,
        sockets: &Sockets,
        request: &SendRequest<'_>,
        cancelled: &impl Fn() -> bool,
        progress: &mut impl FnMut(FaxEvent),
    ) -> Result<Option<TransferStats>> {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        if self.started.elapsed() > Duration::from_secs(30 * 60) {
            return Err(Error::Sip("Fax exceeded the 30-minute call limit".into()));
        }
        if self.connected.is_none() && self.started.elapsed() > Duration::from_secs(60) {
            return Err(Error::Sip(
                "SIP call was not answered within 60 seconds".into(),
            ));
        }
        sockets.poll_mappings()?;
        SELECTED.set(sip.id);
        for event in drain_events() {
            match event {
                Event::Connected => {
                    progress(FaxEvent::Stage(FaxStage::Connected));
                    tracing::debug!("SIP dialog confirmed");
                    self.connected.get_or_insert_with(Instant::now);
                }
                Event::Disconnected(code, reason) => {
                    tracing::debug!(code, %reason, "SIP dialog disconnected");
                    if request.mode == Mode::Auto
                        && !self.receiving
                        && self.connected.is_none()
                        && code == 488
                        && !self.direct_retry
                    {
                        self.direct_retry = true;
                        self.remote = None;
                        self.audio = None;
                        self.packets = None;
                        sip.release_invite();
                        sip.invite(request, Kind::T38)?;
                        continue;
                    }
                    return Err(Error::Sip(format!(
                        "Call ended before T.30 completion: {code} {reason}"
                    )));
                }
                Event::PeerHangup(reason) => {
                    return Err(Error::Sip(format!(
                        "Peer ended call before T.30 completion: {reason}"
                    )));
                }
                Event::Media(description) => {
                    let negotiated = sdp::remote(&description)?;
                    tracing::debug!(?negotiated, "Negotiated remote media");
                    if self.remote.as_ref() == Some(&negotiated) {
                        continue;
                    }
                    if self.fax.is_some() {
                        return Err(Error::Sip(
                            "Peer changed media after fax transmission started".into(),
                        ));
                    }
                    match negotiated {
                        RemoteMedia::Audio { address, codec } => {
                            self.audio = Some(AudioNetwork::new(
                                &sockets.audio,
                                address,
                                codec,
                                sockets.automatic_nat,
                                sockets.audio_mapping.clone(),
                            )?);
                        }
                        RemoteMedia::T38 {
                            address,
                            max_datagram,
                            ..
                        } => {
                            self.packets = Some(PacketNetwork::new(
                                &sockets.fax,
                                address,
                                max_datagram,
                                sockets.automatic_nat,
                                sockets.fax_mapping.clone(),
                            )?);
                        }
                    }
                    self.remote = Some(negotiated);
                }
                Event::InviteStarted(cseq) => {
                    if let Attempt::Waiting { cseq: current, .. } = &mut self.attempt {
                        *current = cseq;
                    }
                }
                Event::InviteFinal(cseq, code) => {
                    tracing::debug!(cseq, code, "INVITE transaction final response");
                    if matches!(self.attempt, Attempt::Waiting { cseq: expected, .. } if expected == cseq)
                        && code >= 300
                        && !matches!(code, 401 | 407)
                    {
                        tracing::info!(code, "T.38 offer rejected; continuing with G.711");
                        self.attempt = Attempt::Rejected;
                    }
                }
                Event::MediaError(reason) => {
                    tracing::warn!(%reason, "Media negotiation error");
                    if !matches!(self.attempt, Attempt::Waiting { .. }) {
                        return Err(Error::Sip(reason));
                    }
                }
                Event::Registered(..) | Event::Flow(..) => (),
                Event::Ringing => progress(FaxEvent::Stage(FaxStage::Ringing)),
            }
        }
        let Some(answered) = self.connected else {
            return Ok(None);
        };
        if self.fax.is_none() {
            match self.remote.as_ref() {
                Some(RemoteMedia::T38 { bit_rate, .. }) => {
                    let mut terminal = match self.receiving { true => PacketFax::receiver_with_ecm(request.file, request.station_id, self.ecm)?, false => PacketFax::transmitter(request.file, request.station_id)? };
                    terminal.limit_bit_rate(*bit_rate)?;
                    self.fax = Some(FaxMedia::Packets(terminal));
                    progress(FaxEvent::Stage(FaxStage::Negotiating));
                    set_switch_allowed(false);
                    tracing::info!("Sending fax over T.38");
                }
                Some(RemoteMedia::Audio { .. }) => match request.mode {
                    Mode::T38 => return Err(Error::Sip("Peer did not accept T.38".into())),
                    Mode::G711 => {
                        self.fax = Some(FaxMedia::Audio(match self.receiving { true => AudioFax::receiver_with_ecm(request.file, request.station_id, self.ecm)?, false => AudioFax::transmitter(request.file, request.station_id)? }));
                        set_switch_allowed(false);
                        progress(FaxEvent::Stage(FaxStage::Negotiating));
                    }
                    Mode::Auto => match &mut self.attempt {
                        Attempt::NotStarted if (self.receiving || answered.elapsed() >= Duration::from_millis(3500)) => {
                            progress(FaxEvent::Stage(FaxStage::OfferingT38));
                            let cseq = sip.reinvite()?;
                            self.attempt = Attempt::Waiting { cseq, since: Instant::now(), cancelling: false };
                            tracing::info!("Requesting T.38 upgrade");
                        }
                        Attempt::Rejected => {
                            progress(FaxEvent::Stage(FaxStage::FallingBack));
                            self.fax = Some(FaxMedia::Audio(match self.receiving { true => AudioFax::receiver_with_ecm(request.file, request.station_id, self.ecm)?, false => AudioFax::transmitter(request.file, request.station_id)? }));
                            set_switch_allowed(false);
                            progress(FaxEvent::Stage(FaxStage::Negotiating));
                        }
                        Attempt::Waiting { since, cancelling, .. } if since.elapsed() >= Duration::from_secs(5) && !*cancelling => {
                            tracing::warn!("T.38 offer timed out; cancelling re-INVITE before G.711 fallback");
                            sip.cancel_reinvite()?;
                            *cancelling = true;
                        }
                        Attempt::Waiting { since, .. } if since.elapsed() > Duration::from_secs(38) => return Err(Error::Sip("T.38 re-INVITE did not settle; refusing an ambiguous audio fallback".into())),
                        _ => (),
                    },
                },
                None if answered.elapsed() > Duration::from_secs(5) => return Err(Error::Sip("Call connected without usable media".into())),
                None => (),
            }
        }
        let now = Instant::now();
        if now < self.next_frame {
            return Ok(None);
        }
        if now.duration_since(self.next_frame) > Duration::from_millis(250) {
            tracing::warn!(
                late_ms = now.duration_since(self.next_frame).as_millis(),
                "Fax media clock fell behind"
            );
            self.next_frame = now;
        }
        self.next_frame += Duration::from_millis(20);
        let events = match (&mut self.fax, &mut self.audio, &mut self.packets) {
            (Some(FaxMedia::Audio(modem)), Some(network), _) => {
                let mut frame = network.receive()?;
                modem.receive(&mut frame);
                network.send(modem.transmit())?;
                if network.last_received.elapsed() > Duration::from_secs(30) {
                    return Err(Error::Sip("No incoming RTP for 30 seconds".into()));
                }
                modem.events()?
            }
            (Some(FaxMedia::Packets(terminal)), _, Some(network)) => {
                for packet in network.receive()? {
                    terminal.receive(packet.sequence, &packet.payload)?;
                }
                terminal.tick();
                for packet in terminal.packets() {
                    network.send(packet)?;
                }
                if !self.receiving
                    && network.transmitted_bytes != self.reported_bytes
                    && self.last_activity_report.elapsed() >= Duration::from_millis(250)
                {
                    self.reported_bytes = network.transmitted_bytes;
                    self.last_activity_report = now;
                    progress(FaxEvent::T38Activity {
                        transmitted_bytes: self.reported_bytes,
                    });
                }
                // SpanDSP's T.30 timers enforce protocol response deadlines;
                // this watchdog detects inactivity in both media directions.
                if network.last_activity.elapsed() > Duration::from_secs(30) {
                    return Err(Error::Sip("No T.38 media activity for 30 seconds".into()));
                }
                terminal.events()?
            }
            (None, Some(network), _) => {
                network.receive()?;
                let frame = std::array::from_fn(|_| {
                    let index = self.cng_samples;
                    self.cng_samples += 1;
                    match !self.receiving && index % 28000 < 4000 {
                        true => {
                            (7000.0
                                * (index as f64 * std::f64::consts::TAU * 1100.0 / 8000.0).sin())
                                as i16
                        }
                        false => 0,
                    }
                });
                network.send(frame)?;
                Vec::new()
            }
            _ => Vec::new(),
        };
        for event in events {
            if matches!(event, FaxEvent::Negotiated) {
                progress(FaxEvent::Stage(match self.fax {
                    Some(FaxMedia::Packets(_)) => FaxStage::SendingT38,
                    _ => FaxStage::SendingG711,
                }));
            }
            tracing::debug!(?event, "Fax protocol event");
            match event {
                FaxEvent::Completed(outcome) => {
                    progress(FaxEvent::Completed(outcome.clone()));
                    let stats = outcome?;
                    return Ok(Some(stats));
                }
                event => progress(event),
            }
        }
        Ok(None)
    }
}

fn run_call(
    sip: &mut Sip,
    sockets: &Sockets,
    request: &SendRequest<'_>,
    cancelled: &impl Fn() -> bool,
    progress: &mut impl FnMut(FaxEvent),
) -> Result<TransferStats> {
    let mut call = CallPump::new(false, true);
    loop {
        sip.poll()?;
        if let Some(stats) = call.step(sip, sockets, request, cancelled, progress)? {
            return Ok(stats);
        }
    }
}

enum Event {
    Ringing,
    Connected,
    Disconnected(i32, String),
    PeerHangup(String),
    Media(String),
    MediaError(String),
    InviteStarted(i32),
    InviteFinal(i32, i32),
    Registered(i32, String),
    Flow(TransportRef, Option<(String, u16)>, pj::pj_sockaddr),
}
struct Callbacks {
    local: LocalMedia,
    mode: Mode,
    switch_allowed: bool,
    current: Kind,
    events: VecDeque<Event>,
}
thread_local! {
    static CALLBACKS: RefCell<HashMap<usize, Callbacks>> = RefCell::new(HashMap::new());
    static SELECTED: Cell<usize> = const { Cell::new(0) };
    static INVITES: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<usize> = const { Cell::new(1) };
    static ENDPOINT: RefCell<std::rc::Weak<Endpoint>> = const { RefCell::new(std::rc::Weak::new()) };
}
fn select_invite(inv: *mut pj::pjsip_inv_session) {
    if let Some(id) = INVITES.with(|map| map.borrow().get(&(inv as usize)).copied()) {
        SELECTED.set(id);
    }
}
fn push(event: Event) {
    CALLBACKS.with(|slot| {
        if let Some(callbacks) = slot.borrow_mut().get_mut(&SELECTED.get()) {
            callbacks.events.push_back(event);
        }
    });
}
fn drain_events() -> Vec<Event> {
    CALLBACKS.with(|slot| {
        slot.borrow_mut()
            .get_mut(&SELECTED.get())
            .map(|callbacks| callbacks.events.drain(..).collect())
            .unwrap_or_default()
    })
}
fn set_switch_allowed(allowed: bool) {
    CALLBACKS.with(|slot| {
        if let Some(callbacks) = slot.borrow_mut().get_mut(&SELECTED.get()) {
            callbacks.switch_allowed = allowed;
        }
    });
}

struct Endpoint {
    tls_owners: RefCell<Vec<Rc<std::pin::Pin<Box<crate::tls::Transport>>>>>,
    caching_pool: Box<pj::pj_caching_pool>,
    endpt: *mut pj::pjsip_endpoint,
    log_module: Box<pj::pjsip_module>,
    options_module: Box<pj::pjsip_module>,
    _logging: PjLogging,
    _thread: PhantomData<Rc<()>>,
}
struct TransportRef(*mut pj::pjsip_transport);
impl Clone for TransportRef {
    fn clone(&self) -> Self {
        unsafe {
            pj::pjsip_transport_add_ref(self.0);
        }
        Self(self.0)
    }
}
impl Drop for TransportRef {
    fn drop(&mut self) {
        unsafe {
            pj::pjsip_transport_dec_ref(self.0);
        }
    }
}
struct NetworkOwner {
    selector: pj::pjsip_tpselector,
    held: Option<TransportRef>,
    _endpoint: Rc<Endpoint>,
}
impl Drop for NetworkOwner {
    fn drop(&mut self) {
        unsafe {
            if let Some(held) = &self.held {
                pj::pjsip_transport_shutdown2(held.0, 1);
            }
            if self.selector.type_ == pj::pjsip_tpselector_type_PJSIP_TPSELECTOR_LISTENER {
                let listener = self.selector.u.listener;
                if !listener.is_null()
                    && let Some(destroy) = (*listener).destroy
                {
                    destroy(listener);
                }
            }
        }
        self.held.take();
    }
}
struct Sip {
    network: Option<Rc<NetworkOwner>>,
    flow: Option<TransportRef>,
    endpoint: Rc<Endpoint>,
    id: usize,
    endpt: *mut pj::pjsip_endpoint,
    pool: *mut pj::pj_pool_t,
    inv: *mut pj::pjsip_inv_session,
    registration: *mut pj::pjsip_regc,
    selector: pj::pjsip_tpselector,
    contact: String,
    identity: String,
    registered: bool,
    tls: Option<Rc<std::pin::Pin<Box<crate::tls::Transport>>>>,
}

impl Endpoint {
    fn get() -> Result<Rc<Self>> {
        if let Some(endpoint) = ENDPOINT.with(|slot| slot.borrow().upgrade()) {
            return Ok(endpoint);
        }
        unsafe {
            let logging = PjLogging::install();
            check(pj::pj_init())?;
            let mut caching_pool = Box::<pj::pj_caching_pool>::default();
            pj::pj_caching_pool_init(
                caching_pool.as_mut(),
                &raw const pj::pj_pool_factory_default_policy,
                0,
            );
            let mut endpoint = Self {
                tls_owners: RefCell::new(Vec::new()),
                caching_pool,
                endpt: ptr::null_mut(),
                log_module: logging::sip_module(),
                options_module: Box::new(pj::pjsip_module {
                    name: pjstr("faxe-incoming"),
                    id: -1,
                    priority: pj::pjsip_module_priority_PJSIP_MOD_PRIORITY_UA_PROXY_LAYER as i32
                        - 1,
                    on_rx_request: Some(on_options),
                    ..Default::default()
                }),
                _logging: logging,
                _thread: PhantomData,
            };
            check(pj::pjlib_util_init())?;
            pj::pjsip_sip_cfg_var.tcp.keep_alive_interval = 15;
            pj::pjsip_sip_cfg_var.endpt.req_has_via_alias = 1;
            check(pj::pjsip_endpt_create(
                &mut endpoint.caching_pool.factory,
                c"faxe".as_ptr(),
                &mut endpoint.endpt,
            ))?;
            check(pj::pjsip_endpt_register_module(
                endpoint.endpt,
                endpoint.log_module.as_mut(),
            ))?;
            check(pj::pjsip_tsx_layer_init_module(endpoint.endpt))?;
            check(pj::pjsip_ua_init_module(endpoint.endpt, ptr::null()))?;
            let callbacks = pj::pjsip_inv_callback {
                on_state_changed: Some(on_state),
                on_new_session: Some(on_new_session),
                on_media_update: Some(on_media),
                on_rx_offer: Some(on_offer),
                on_tsx_state_changed: Some(on_transaction),
                ..Default::default()
            };
            check(pj::pjsip_inv_usage_init(endpoint.endpt, &callbacks))?;
            check(pj::pjsip_100rel_init_module(endpoint.endpt))?;
            check(pj::pjsip_endpt_register_module(
                endpoint.endpt,
                endpoint.options_module.as_mut(),
            ))?;
            let endpoint = Rc::new(endpoint);
            ENDPOINT.with(|slot| *slot.borrow_mut() = Rc::downgrade(&endpoint));
            Ok(endpoint)
        }
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        unsafe {
            for transport in self.tls_owners.get_mut().iter() {
                transport.close_io();
            }
            if !self.endpt.is_null() {
                pj::pjsip_endpt_destroy(self.endpt);
            }
            self.tls_owners.get_mut().clear();
            pj::pj_caching_pool_destroy(self.caching_pool.as_mut());
            pj::pj_shutdown();
        }
    }
}

impl Sip {
    fn new(
        account: &Account<'_>,
        local: LocalMedia,
        mode: Mode,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Self> {
        Self::new_prepared(account, local, mode, cancelled, None)
    }
    fn new_prepared(
        account: &Account<'_>,
        local: LocalMedia,
        mode: Mode,
        cancelled: &impl Fn() -> bool,
        prepared: Option<crate::tls::Prepared>,
    ) -> Result<Self> {
        let endpoint = Endpoint::get()?;
        let id = NEXT_ID.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        SELECTED.set(id);
        unsafe {
            let mut sip = Self {
                network: None,
                flow: None,
                endpt: endpoint.endpt,
                endpoint,
                id,
                pool: ptr::null_mut(),
                inv: ptr::null_mut(),
                registration: ptr::null_mut(),
                selector: pj::pjsip_tpselector::default(),
                contact: String::new(),
                identity: String::new(),
                registered: false,
                tls: None,
            };
            sip.pool = pj::pjsip_endpt_create_pool(sip.endpt, c"faxe-account".as_ptr(), 4096, 4096);
            if sip.pool.is_null() {
                return Err(Error::Sip("Cannot allocate account pool".into()));
            }
            let mut address = pj::pj_sockaddr::default();
            let ip = local.ip.to_string();
            check(pj::pj_sockaddr_init(
                pj::PJ_AF_INET as i32,
                &mut address,
                &pjstr(&ip),
                0,
            ))?;
            let (port, parameter) = match account.transport {
                SignalingTransport::Udp => {
                    let mut transport = ptr::null_mut();
                    check(pj::pjsip_udp_transport_start(
                        sip.endpt,
                        &address.ipv4,
                        ptr::null(),
                        1,
                        &mut transport,
                    ))?;
                    sip.selector.type_ = pj::pjsip_tpselector_type_PJSIP_TPSELECTOR_TRANSPORT;
                    sip.selector.u.transport = transport;
                    ((*transport).local_name.port, "udp")
                }
                SignalingTransport::Tcp => {
                    let mut factory = ptr::null_mut();
                    check(pj::pjsip_tcp_transport_start(
                        sip.endpt,
                        &address.ipv4,
                        1,
                        &mut factory,
                    ))?;
                    sip.selector.type_ = pj::pjsip_tpselector_type_PJSIP_TPSELECTOR_LISTENER;
                    sip.selector.u.listener = factory;
                    ((*factory).addr_name.port, "tcp")
                }
                SignalingTransport::Tls => {
                    let (host, port) = tls_peer(account)?;
                    let transport = match prepared {
                        Some(prepared) => crate::tls::Transport::install(sip.endpt, prepared)?,
                        None => crate::tls::Transport::connect(sip.endpt, &host, port, cancelled)?,
                    };
                    sip.selector = transport.selector();
                    let port = transport.port();
                    let transport = Rc::new(Box::into_pin(transport));
                    sip.endpoint
                        .tls_owners
                        .borrow_mut()
                        .push(Rc::clone(&transport));
                    sip.tls = Some(transport);
                    (port as i32, "tls")
                }
            };
            let held = if sip.selector.type_ == pj::pjsip_tpselector_type_PJSIP_TPSELECTOR_TRANSPORT
            {
                let transport = sip.selector.u.transport;
                // Rustls connect already creates the owning reference.
                if account.transport != SignalingTransport::Tls {
                    check(pj::pjsip_transport_add_ref(transport))?;
                }
                Some(TransportRef(transport))
            } else {
                None
            };
            sip.network = Some(Rc::new(NetworkOwner {
                selector: sip.selector,
                held,
                _endpoint: sip.endpoint.clone(),
            }));
            sip.contact = format!(
                "<sip:{}@{}:{port};transport={parameter}>",
                account.username, local.ip
            );
            sip.identity = format!(
                "sip:{}@{}:{}",
                account.username, account.server, account.port
            );
            CALLBACKS.with(|slot| {
                slot.borrow_mut().insert(
                    id,
                    Callbacks {
                        local,
                        mode,
                        switch_allowed: mode == Mode::Auto,
                        current: match mode {
                            Mode::T38 => Kind::T38,
                            _ => Kind::Audio,
                        },
                        events: VecDeque::new(),
                    },
                );
            });
            Ok(sip)
        }
    }

    fn start_registration(&mut self, account: &Account<'_>) -> Result<()> {
        unsafe {
            check(pj::pjsip_regc_create(
                self.endpt,
                self.id as *mut std::ffi::c_void,
                Some(on_registration),
                &mut self.registration,
            ))?;
            let parameter = match account.transport {
                SignalingTransport::Udp => "udp",
                SignalingTransport::Tcp => "tcp",
                SignalingTransport::Tls => "tls",
            };
            let server = format!(
                "sip:{}:{};transport={parameter}",
                account.server, account.port
            );
            check(pj::pjsip_regc_init(
                self.registration,
                &pjstr(&server),
                &pjstr(&self.identity),
                &pjstr(&self.identity),
                1,
                &pjstr(&self.contact),
                300,
            ))?;
            check(pj::pjsip_regc_set_transport(
                self.registration,
                &self.selector,
            ))?;
            if let Some(credential) = credential(account) {
                check(pj::pjsip_regc_set_credentials(
                    self.registration,
                    1,
                    &credential,
                ))?;
            }
            if let Some(proxy) = account.outbound_proxy {
                let route = route_set(self.pool, &route_uri(account, proxy)?)?;
                check(pj::pjsip_regc_set_route_set(self.registration, route))?;
            }
            let mut data = ptr::null_mut();
            check(pj::pjsip_regc_register(self.registration, 1, &mut data))?;
            check(pj::pjsip_regc_send(self.registration, data))?;
        }
        Ok(())
    }
    fn register(&mut self, account: &Account<'_>, cancelled: &impl Fn() -> bool) -> Result<()> {
        self.start_registration(account)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            self.poll()?;
            SELECTED.set(self.id);
            for event in drain_events() {
                if let Event::Registered(code, reason) = event {
                    tracing::debug!(code, %reason, "SIP registration response");
                    match code {
                        200..300 => {
                            self.registered = true;
                            return Ok(());
                        }
                        _ => {
                            return Err(Error::Sip(format!(
                                "Registration failed: {code} {reason}"
                            )));
                        }
                    }
                }
            }
        }
        Err(Error::Sip("SIP registration timed out".into()))
    }

    fn invite(&mut self, request: &SendRequest<'_>, kind: Kind) -> Result<()> {
        SELECTED.set(self.id);
        let account = &request.account;
        let parameter = match account.transport {
            SignalingTransport::Udp => "udp",
            SignalingTransport::Tcp => "tcp",
            SignalingTransport::Tls => "tls",
        };
        let target = match request.destination.starts_with("sip:")
            || request.destination.starts_with("sips:")
        {
            true => request.destination.to_owned(),
            false => format!(
                "sip:{}@{}:{};transport={parameter}",
                request.destination.replace('#', "%23"),
                account.server,
                account.port
            ),
        };
        let target = route_uri(account, &target)?;
        unsafe {
            let mut dialog = ptr::null_mut();
            check(pj::pjsip_dlg_create_uac(
                pj::pjsip_ua_instance(),
                &pjstr(&self.identity),
                &pjstr(&self.contact),
                &pjstr(&target),
                &pjstr(&target),
                &mut dialog,
            ))?;
            pj::pjsip_dlg_inc_lock(dialog);
            let outcome = (|| {
                check(pj::pjsip_dlg_set_transport(dialog, &self.selector))?;
                if let Some(credential) = credential(account) {
                    check(pj::pjsip_auth_clt_set_credentials(
                        &mut (*dialog).auth_sess,
                        1,
                        &credential,
                    ))?;
                }
                if let Some(proxy) = account.outbound_proxy {
                    check(pj::pjsip_dlg_set_route_set(
                        dialog,
                        route_set(self.pool, &route_uri(account, proxy)?)?,
                    ))?;
                }
                let offer = CALLBACKS.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    let callbacks = slot
                        .get_mut(&SELECTED.get())
                        .expect("call callbacks installed");
                    callbacks.current = kind;
                    callbacks.local.version += 1;
                    callbacks.local.offer(kind)
                });
                let sdp = parse_sdp((*dialog).pool, &offer)?;
                check(pj::pjsip_inv_create_uac(dialog, sdp, 0, &mut self.inv))?;
                check(pj::pjsip_inv_add_ref(self.inv))?;
                INVITES.with(|map| map.borrow_mut().insert(self.inv as usize, self.id));
                let mut data = ptr::null_mut();
                check(pj::pjsip_inv_invite(self.inv, &mut data))?;
                check(pj::pjsip_inv_send_msg(self.inv, data))
            })();
            pj::pjsip_dlg_dec_lock(dialog);
            outcome
        }
    }

    fn reinvite(&mut self) -> Result<i32> {
        SELECTED.set(self.id);
        let offer = CALLBACKS.with(|slot| {
            let mut slot = slot.borrow_mut();
            let callbacks = slot
                .get_mut(&SELECTED.get())
                .expect("call callbacks installed");
            callbacks.local.version += 1;
            callbacks.local.offer(Kind::T38)
        });
        unsafe {
            let sdp = parse_sdp((*self.inv).pool_prov, &offer)?;
            let mut data = ptr::null_mut();
            check(pj::pjsip_inv_reinvite(
                self.inv,
                ptr::null(),
                sdp,
                &mut data,
            ))?;
            let header =
                pj::pjsip_msg_find_hdr((*data).msg, pj::pjsip_hdr_e_PJSIP_H_CSEQ, ptr::null())
                    .cast::<pj::pjsip_cseq_hdr>();
            let cseq = (*header).cseq;
            check(pj::pjsip_inv_send_msg(self.inv, data))?;
            Ok(cseq)
        }
    }

    fn cancel_reinvite(&mut self) -> Result<()> {
        unsafe {
            let mut data = ptr::null_mut();
            check(pj::pjsip_inv_cancel_reinvite(self.inv, &mut data))?;
            if !data.is_null() {
                check(pj::pjsip_inv_send_msg(self.inv, data))?;
            }
        }
        Ok(())
    }

    fn poll(&self) -> Result<()> {
        if let Some(tls) = &self.tls {
            tls.poll()?;
        }
        unsafe {
            check(pj::pjsip_endpt_handle_events(
                self.endpt,
                &pj::pj_time_val { sec: 0, msec: 10 },
            ))
        }
    }

    fn release_invite(&mut self) {
        SELECTED.set(self.id);
        if !self.inv.is_null() {
            unsafe {
                pj::pjsip_inv_terminate(self.inv, 487, 0);
                pj::pjsip_inv_dec_ref(self.inv);
            }
            INVITES.with(|map| map.borrow_mut().remove(&(self.inv as usize)));
            self.inv = ptr::null_mut();
        }
    }

    fn end_call(&mut self) {
        unsafe {
            if !self.inv.is_null()
                && (*self.inv).state != pj::pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED
            {
                let mut data = ptr::null_mut();
                if pj::pjsip_inv_end_session(self.inv, 603, ptr::null(), &mut data) == 0
                    && !data.is_null()
                {
                    pj::pjsip_inv_send_msg(self.inv, data);
                }
            }
        }
    }
    fn unregister(&mut self) {
        unsafe {
            if self.registered {
                let mut data = ptr::null_mut();
                if pj::pjsip_regc_unregister(self.registration, &mut data) == 0 {
                    pj::pjsip_regc_send(self.registration, data);
                }
            }
        }
    }
    fn finish(&mut self) {
        self.end_call();
        self.unregister();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.poll().is_err() {
                break;
            }
        }
    }
}

impl Drop for Sip {
    fn drop(&mut self) {
        self.release_invite();
        unsafe {
            if !self.registration.is_null() {
                pj::pjsip_regc_destroy2(self.registration, 1);
            }
            if !self.pool.is_null() {
                pj::pj_pool_release(self.pool);
            }
            // Transports reference endpoint pools. Shutdown and release the account's
            // TLS transport before releasing the shared endpoint owner.
            self.tls.take();
            self.flow.take();
            self.network.take();
        }
        CALLBACKS.with(|slot| slot.borrow_mut().remove(&self.id));
    }
}

pub(crate) fn pjstr(value: &str) -> pj::pj_str_t {
    pj::pj_str_t {
        ptr: value.as_ptr().cast_mut().cast(),
        slen: value.len() as _,
    }
}
pub(crate) fn check(status: pj::pj_status_t) -> Result<()> {
    if status == 0 {
        return Ok(());
    }
    let mut buffer = [0 as std::ffi::c_char; 256];
    let message = unsafe { pj::pj_strerror(status, buffer.as_mut_ptr(), buffer.len() as _) };
    Err(Error::Sip(unsafe { string(message) }))
}
unsafe fn string(value: pj::pj_str_t) -> String {
    if value.ptr.is_null() || value.slen <= 0 {
        return String::new();
    }
    String::from_utf8_lossy(unsafe {
        std::slice::from_raw_parts(value.ptr.cast(), value.slen as usize)
    })
    .into_owned()
}
unsafe fn parse_sdp(
    pool: *mut pj::pj_pool_t,
    source: &str,
) -> Result<*mut pj::pjmedia_sdp_session> {
    unsafe {
        let mut owned = pj::pj_str_t::default();
        pj::pj_strdup_with_null(pool, &mut owned, &pjstr(source));
        let mut sdp = ptr::null_mut();
        check(pj::pjmedia_sdp_parse(
            pool,
            owned.ptr,
            owned.slen as _,
            &mut sdp,
        ))?;
        Ok(sdp)
    }
}
unsafe fn print_sdp(sdp: *const pj::pjmedia_sdp_session) -> Result<String> {
    if sdp.is_null() {
        return Err(Error::Sip("Missing session description".into()));
    }
    let mut buffer = vec![0_u8; 65536];
    let size = unsafe { pj::pjmedia_sdp_print(sdp, buffer.as_mut_ptr().cast(), buffer.len()) };
    if size < 0 {
        return Err(Error::Sip("Session description exceeds 64 KiB".into()));
    }
    Ok(String::from_utf8_lossy(&buffer[..size as usize]).into_owned())
}
fn credential<'a>(account: &'a Account<'a>) -> Option<pj::pjsip_cred_info> {
    account.password.map(|password| pj::pjsip_cred_info {
        realm: pjstr("*"),
        scheme: pjstr("digest"),
        username: pjstr(account.auth_username.unwrap_or(account.username)),
        data_type: 0,
        data: pjstr(password),
        ..Default::default()
    })
}
unsafe fn route_set(pool: *mut pj::pj_pool_t, proxy: &str) -> Result<*mut pj::pjsip_route_hdr> {
    unsafe {
        let mut proxy = proxy.to_owned();
        if !proxy.contains(";lr") {
            proxy.push_str(";lr");
        }
        let mut owned = pj::pj_str_t::default();
        pj::pj_strdup_with_null(pool, &mut owned, &pjstr(&proxy));
        let uri = pj::pjsip_parse_uri(pool, owned.ptr, owned.slen as _, 0);
        if uri.is_null() {
            return Err(Error::Sip("Invalid outbound proxy URI".into()));
        }
        let head = pj::pjsip_route_hdr_create(pool);
        let route = pj::pjsip_route_hdr_create(pool);
        (*head).next = route;
        (*head).prev = route;
        (*route).next = head;
        (*route).prev = head;
        (*route).name_addr.uri = uri;
        Ok(head)
    }
}

unsafe extern "C" fn on_options(data: *mut pj::pjsip_rx_data) -> pj::pj_bool_t {
    unsafe {
        let method = (*(*data).msg_info.msg).line.req.method.id;
        if method == pj::pjsip_method_e_PJSIP_INVITE_METHOD && (*(*data).msg_info.to).tag.slen == 0
        {
            return service::on_incoming(data);
        }
        if method != pj::pjsip_method_e_PJSIP_OPTIONS_METHOD {
            return 0;
        }
        let endpoint = (*(*data).tp_info.transport).endpt;
        match check(pj::pjsip_endpt_respond_stateless(
            endpoint,
            data,
            200,
            ptr::null(),
            ptr::null(),
            ptr::null(),
        )) {
            Ok(()) => tracing::debug!("Answered SIP OPTIONS probe"),
            Err(error) => tracing::warn!(%error, "Could not answer SIP OPTIONS probe"),
        }
        1
    }
}

unsafe fn peer_hangup(event: *const pj::pjsip_event) -> Option<String> {
    unsafe {
        let event = event.as_ref()?;
        let data = match event.type_ {
            pj::pjsip_event_id_e_PJSIP_EVENT_RX_MSG => event.body.rx_msg.rdata,
            pj::pjsip_event_id_e_PJSIP_EVENT_TSX_STATE
                if event.body.tsx_state.type_ == pj::pjsip_event_id_e_PJSIP_EVENT_RX_MSG =>
            {
                event.body.tsx_state.src.rdata
            }
            _ => return None,
        };
        let message = data.as_ref()?.msg_info.msg.as_ref()?;
        if message.type_ != pj::pjsip_msg_type_e_PJSIP_REQUEST_MSG
            || message.line.req.method.id != pj::pjsip_method_e_PJSIP_BYE_METHOD
        {
            return None;
        }
        let mut reasons = Vec::new();
        let mut start = ptr::null();
        loop {
            let header = pj::pjsip_msg_find_hdr_by_name(message, &pjstr("Reason"), start)
                .cast::<pj::pjsip_hdr>();
            if header.is_null() {
                break;
            }
            let mut buffer = [0_u8; 4096];
            let length =
                pj::pjsip_hdr_print_on(header.cast(), buffer.as_mut_ptr().cast(), buffer.len());
            match length {
                0.. => reasons.push(
                    String::from_utf8_lossy(&buffer[..length as usize])
                        .chars()
                        .map(|ch| match ch.is_control() {
                            true => ' ',
                            false => ch,
                        })
                        .collect::<String>(),
                ),
                _ => reasons.push("Reason: [header exceeds diagnostic limit]".into()),
            }
            start = (*header).next.cast();
        }
        Some(match reasons.is_empty() {
            true => "BYE received without a Reason header".into(),
            false => reasons.join("; "),
        })
    }
}

unsafe extern "C" fn on_state(inv: *mut pj::pjsip_inv_session, event: *mut pj::pjsip_event) {
    select_invite(inv);
    unsafe {
        match (*inv).state {
            pj::pjsip_inv_state_PJSIP_INV_STATE_EARLY => push(Event::Ringing),
            pj::pjsip_inv_state_PJSIP_INV_STATE_CONFIRMED => push(Event::Connected),
            pj::pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED => push(match peer_hangup(event) {
                Some(reason) => Event::PeerHangup(reason),
                None => Event::Disconnected((*inv).cause as i32, string((*inv).cause_text)),
            }),
            _ => (),
        }
    }
}
unsafe extern "C" fn on_new_session(inv: *mut pj::pjsip_inv_session, _: *mut pj::pjsip_event) {
    // Do not send the same fax to a forked second dialog.
    unsafe {
        pj::pjsip_inv_terminate(inv, 603, 0);
    }
}
unsafe extern "C" fn on_media(inv: *mut pj::pjsip_inv_session, status: pj::pj_status_t) {
    select_invite(inv);
    let result: Result<String> = (|| unsafe {
        check(status)?;
        let mut remote = ptr::null();
        check(pj::pjmedia_sdp_neg_get_active_remote(
            (*inv).neg,
            &mut remote,
        ))?;
        let description = print_sdp(remote)?;
        if let Ok(media) = sdp::remote(&description) {
            CALLBACKS.with(|slot| {
                if let Some(callbacks) = slot.borrow_mut().get_mut(&SELECTED.get()) {
                    callbacks.current = media.kind();
                }
            });
        }
        Ok(description)
    })();
    push(match result {
        Ok(description) => Event::Media(description),
        Err(error) => Event::MediaError(format!("SDP negotiation failed: {error}")),
    });
}
unsafe extern "C" fn on_offer(
    inv: *mut pj::pjsip_inv_session,
    offer: *const pj::pjmedia_sdp_session,
) {
    select_invite(inv);
    let result = (|| unsafe {
        let description = print_sdp(offer)?;
        let answer = CALLBACKS.with(|slot| {
            let mut slot = slot.borrow_mut();
            let callbacks = slot
                .get_mut(&SELECTED.get())
                .ok_or_else(|| Error::Sip("No active call".into()))?;
            callbacks.local.version += 1;
            let audio = callbacks.mode != Mode::T38
                && (callbacks.switch_allowed || callbacks.current == Kind::Audio);
            let t38 = callbacks.mode != Mode::G711
                && (callbacks.switch_allowed || callbacks.current == Kind::T38);
            callbacks.local.answer(&description, audio, t38)
        })?;
        check(pj::pjsip_inv_set_sdp_answer(
            inv,
            parse_sdp((*inv).pool_prov, &answer)?,
        ))
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "Rejecting incompatible re-INVITE");
    }
}
unsafe extern "C" fn on_transaction(
    inv: *mut pj::pjsip_inv_session,
    transaction: *mut pj::pjsip_transaction,
    _: *mut pj::pjsip_event,
) {
    select_invite(inv);
    unsafe {
        let transaction = &*transaction;
        if transaction.role != pj::pjsip_role_e_PJSIP_ROLE_UAC
            || transaction.method.id != pj::pjsip_method_e_PJSIP_INVITE_METHOD
        {
            return;
        }
        match transaction.state {
            pj::pjsip_tsx_state_e_PJSIP_TSX_STATE_CALLING => {
                push(Event::InviteStarted(transaction.cseq))
            }
            pj::pjsip_tsx_state_e_PJSIP_TSX_STATE_COMPLETED
            | pj::pjsip_tsx_state_e_PJSIP_TSX_STATE_TERMINATED => push(Event::InviteFinal(
                transaction.cseq,
                transaction.status_code,
            )),
            _ => (),
        }
    }
}
unsafe extern "C" fn on_registration(param: *mut pj::pjsip_regc_cbparam) {
    unsafe {
        let param = &*param;
        SELECTED.set(param.token as usize);
        let code = match param.status {
            0 => param.code,
            _ => 0,
        };
        if (200..300).contains(&code) && !param.rdata.is_null() {
            let transport = (*param.rdata).tp_info.transport;
            if !transport.is_null() && pj::pjsip_transport_add_ref(transport) == 0 {
                let via = (*param.rdata).msg_info.via;
                let observed = if !via.is_null() && (*via).rport_param > 0 {
                    let host = string((*via).recvd_param);
                    host.parse::<std::net::Ipv4Addr>()
                        .ok()
                        .map(|_| (host, (*via).rport_param as u16))
                } else {
                    None
                };
                push(Event::Flow(
                    TransportRef(transport),
                    observed,
                    (*param.rdata).pkt_info.src_addr,
                ));
            }
        }
        push(Event::Registered(code, string(param.reason)));
    }
}
