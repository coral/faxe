use crate::{
    Error, Result,
    sip::{check, pjstr},
};
use faxe_pj_sys as pj;
use rustls::{ClientConfig, ClientConnection, RootCertStore, pki_types::ServerName};
use std::{
    cell::{Cell, RefCell, UnsafeCell},
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    ptr,
    sync::Arc,
    time::{Duration, Instant},
};

struct Connection {
    socket: TcpStream,
    tls: ClientConnection,
    failed: Option<String>,
}

const PING_INTERVAL: Duration = Duration::from_secs(25);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);

struct Keepalive {
    last_ping: Instant,
    pending_ping: Option<Instant>,
    // A SIP Supported header may describe a downstream registrar, not this
    // connection's peer. Require a pong only after this peer has answered one
    // of our probes. RFC 5626 section 4.4 permits probes without pong support.
    pong_supported: bool,
    first_crlf: Option<Crlf>,
}

struct Crlf {
    probe: Option<Instant>,
    supported: bool,
}

impl Keepalive {
    fn new(now: Instant) -> Self {
        Self {
            last_ping: now,
            pending_ping: None,
            pong_supported: false,
            first_crlf: None,
        }
    }

    fn next_deadline(&self) -> Instant {
        if self.pong_supported
            && let Some(at) = self.pending_ping
        {
            return at + PONG_TIMEOUT;
        }
        self.last_ping + PING_INTERVAL
    }

    // Two adjacent CRLF tokens are a peer ping, even across TLS reads. A
    // single token can satisfy our probe immediately, but retain its prior
    // state until the next token disambiguates a fragmented double CRLF.
    fn crlf(&mut self) -> bool {
        if let Some(first) = self.first_crlf.take() {
            if first.probe.is_some() {
                self.pending_ping = first.probe;
                self.pong_supported = first.supported;
            }
            return true;
        }
        self.first_crlf = Some(Crlf {
            probe: self.pending_ping.take(),
            supported: self.pong_supported,
        });
        if self.first_crlf.as_ref().unwrap().probe.is_some() {
            self.pong_supported = true;
        }
        false
    }

    fn message(&mut self) {
        self.first_crlf = None;
    }

    // Called only AFTER processing received bytes, including a queued pong.
    fn tick(&mut self, now: Instant) -> Result<bool> {
        if self.pong_supported && self.pending_ping.is_some_and(|at| now >= at + PONG_TIMEOUT) {
            return Err(Error::Sip("TLS keepalive pong timed out".into()));
        }
        if now >= self.last_ping + PING_INTERVAL {
            // Pongs to distinct probes are not a double CRLF. Whitespace
            // received before any probe may still be half of a peer ping.
            if self
                .first_crlf
                .as_ref()
                .is_some_and(|first| first.probe.is_some())
            {
                self.first_crlf = None;
            }
            self.last_ping = now;
            self.pending_ping = Some(now);
            return Ok(true);
        }
        Ok(false)
    }
}

impl Connection {
    fn pump(&mut self) -> Result<Vec<u8>> {
        if let Some(error) = &self.failed {
            return Err(Error::Sip(error.clone()));
        }
        while self.tls.wants_write() {
            match self.tls.write_tls(&mut self.socket) {
                Ok(0) => return Err(Error::Sip("TLS connection closed during write".into())),
                Ok(_) => (),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        let mut plaintext = Vec::new();
        for _ in 0..16 {
            match self.tls.read_tls(&mut self.socket) {
                Ok(0) => {
                    self.failed = Some("TLS peer closed the connection".into());
                    break;
                }
                Ok(_) => {
                    let state = self
                        .tls
                        .process_new_packets()
                        .map_err(|e| Error::Sip(format!("TLS: {e}")))?;
                    if state.peer_has_closed() {
                        self.failed = Some("TLS peer closed the connection".into());
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => {
                    self.failed = Some(format!("TLS receive failed: {error}"));
                    break;
                }
            }
            let mut bytes = [0_u8; 4096];
            loop {
                match self.tls.reader().read(&mut bytes) {
                    Ok(0) => break,
                    Ok(size) => plaintext.extend_from_slice(&bytes[..size]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error.into()),
                }
            }
            if plaintext.len() > 65536 {
                return Err(Error::Sip("TLS receive buffer limit exceeded".into()));
            }
            if self.failed.is_some() {
                break;
            }
        }
        if plaintext.is_empty()
            && let Some(error) = &self.failed
        {
            return Err(Error::Sip(error.clone()));
        }
        Ok(plaintext)
    }
}

/// Rust socket/TLS state only. No PJSIP handles are created until installation
/// on the SIP thread, so slow DNS/connect/certificate work cannot stall media.
pub(crate) struct Prepared {
    io: Connection,
    incoming: Vec<u8>,
    local: SocketAddr,
    remote: SocketAddr,
    host: String,
    port: u16,
}

/// Owned by the SIP worker; native callbacks borrow only the connection cell.
#[repr(C)]
pub(crate) struct Transport {
    // PJSIP's transport ABI requires the native header at offset zero.
    base: UnsafeCell<pj::pjsip_transport>,
    io: RefCell<Connection>,
    incoming: RefCell<Vec<u8>>,
    receive_pool: *mut pj::pj_pool_t,
    destroyed: Cell<bool>,
    keepalive: RefCell<Keepalive>,
    reactor: Option<crate::sip::io::Io>,
}

impl Transport {
    pub unsafe fn connect(
        endpoint: *mut pj::pjsip_endpoint,
        host: &str,
        port: u16,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Box<Self>> {
        let prepared = Self::prepare(host, port, cancelled)?;
        unsafe { Self::install(endpoint, prepared) }
    }
    pub(crate) fn prepare(
        host: &str,
        port: u16,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Prepared> {
        #[cfg(test)]
        if let Some(config) = tests::TRUST.with(|trust| trust.borrow().clone()) {
            return Self::prepare_with_config(host, port, cancelled, config);
        }
        let certificates = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        for cert in certificates.certs {
            roots
                .add(cert)
                .map_err(|e| Error::Sip(format!("TLS trust store: {e}")))?;
        }
        if roots.is_empty() {
            return Err(Error::Sip(
                "No system TLS trust anchors are available".into(),
            ));
        }
        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| Error::Sip(e.to_string()))?
                .with_root_certificates(roots)
                .with_no_client_auth();
        Self::prepare_with_config(host, port, cancelled, Arc::new(config))
    }

    fn prepare_with_config(
        host: &str,
        port: u16,
        cancelled: &impl Fn() -> bool,
        config: Arc<ClientConfig>,
    ) -> Result<Prepared> {
        let name = ServerName::try_from(host.to_owned())
            .map_err(|e| Error::Sip(format!("Invalid TLS server name: {e}")))?;
        let remote = (host, port)
            .to_socket_addrs()?
            .find(SocketAddr::is_ipv4)
            .ok_or_else(|| Error::Sip("No IPv4 TLS server address".into()))?;
        let socket = TcpStream::connect_timeout(&remote, Duration::from_secs(10))?;
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        let local = socket.local_addr()?;
        let mut io = Connection {
            socket,
            tls: ClientConnection::new(config, name).map_err(|e| Error::Sip(e.to_string()))?,
            failed: None,
        };
        io.tls.set_buffer_limit(Some(65536));
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut incoming = Vec::new();
        while io.tls.is_handshaking() {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            if Instant::now() > deadline {
                return Err(Error::Sip("TLS handshake timed out".into()));
            }
            incoming.extend(io.pump()?);
            std::thread::sleep(Duration::from_millis(5));
        }
        tracing::info!(server = host, port, protocol = ?io.tls.protocol_version(), "SIP TLS certificate verified");
        Ok(Prepared {
            io,
            incoming,
            local,
            remote,
            host: host.to_owned(),
            port,
        })
    }
    pub(crate) unsafe fn install(
        endpoint: *mut pj::pjsip_endpoint,
        prepared: Prepared,
    ) -> Result<Box<Self>> {
        let Prepared {
            io,
            incoming,
            local,
            remote,
            host,
            port,
        } = prepared;
        unsafe {
            let pool = pj::pjsip_endpt_create_pool(endpoint, c"faxe-rustls".as_ptr(), 4096, 4096);
            if pool.is_null() {
                return Err(Error::Sip("Could not allocate TLS transport".into()));
            }
            let mut transport = Box::new(Self {
                base: UnsafeCell::new(pj::pjsip_transport::default()),
                io: RefCell::new(io),
                incoming: RefCell::new(incoming),
                receive_pool: ptr::null_mut(),
                destroyed: Cell::new(false),
                keepalive: RefCell::new(Keepalive::new(Instant::now())),
                reactor: None,
            });
            let base = transport.base.get_mut();
            base.pool = pool;
            check(pj::pj_atomic_create(pool, 0, &mut base.ref_cnt))?;
            check(pj::pj_lock_create_recursive_mutex(
                pool,
                c"faxe-rustls".as_ptr(),
                &mut base.lock,
            ))?;
            base.key.type_ = pj::pjsip_transport_type_e_PJSIP_TRANSPORT_TLS as _;
            base.key.rem_addr = address(remote)?;
            base.local_addr = address(local)?;
            base.addr_len = pj::pj_sockaddr_get_len(ptr::from_ref(&base.local_addr).cast()) as _;
            base.type_name = c"TLS".as_ptr().cast_mut();
            base.info = c"Rustls verified SIP connection".as_ptr().cast_mut();
            base.flag = (pj::pjsip_transport_flags_e_PJSIP_TRANSPORT_RELIABLE
                | pj::pjsip_transport_flags_e_PJSIP_TRANSPORT_SECURE) as _;
            base.dir = pj::pjsip_transport_dir_PJSIP_TP_DIR_OUTGOING;
            pj::pj_strdup_with_null(
                pool,
                &mut base.local_name.host,
                &pjstr(&local.ip().to_string()),
            );
            base.local_name.port = local.port() as _;
            pj::pj_strdup_with_null(pool, &mut base.remote_name.host, &pjstr(&host));
            base.remote_name.port = port as _;
            base.endpt = endpoint;
            base.tpmgr = pj::pjsip_endpt_get_tpmgr(endpoint);
            base.send_msg = Some(send);
            base.do_shutdown = Some(shutdown);
            base.destroy = Some(destroy);
            transport.receive_pool =
                pj::pjsip_endpt_create_pool(endpoint, c"faxe-tls-rx".as_ptr(), 8192, 8192);
            if transport.receive_pool.is_null() {
                return Err(Error::Sip("Could not allocate TLS receive pool".into()));
            }
            transport.reactor = Some(crate::sip::io::Io::stream(
                endpoint,
                &transport.io.borrow().socket,
            )?);
            check(pj::pjsip_transport_add_ref(transport.base.get()))?;
            check(pj::pjsip_transport_register(
                (*transport.base.get()).tpmgr,
                transport.base.get(),
            ))?;
            Ok(transport)
        }
    }

    pub fn selector(&self) -> pj::pjsip_tpselector {
        let mut selector = pj::pjsip_tpselector {
            type_: pj::pjsip_tpselector_type_PJSIP_TPSELECTOR_TRANSPORT,
            ..Default::default()
        };
        selector.u.transport = self.base.get();
        selector
    }

    pub(crate) fn destroyed(&self) -> bool {
        self.destroyed.get()
    }

    pub fn port(&self) -> u16 {
        unsafe { (*self.base.get()).local_name.port as u16 }
    }

    pub(crate) fn close_io(&self) {
        if let Some(io) = &self.reactor {
            io.close();
        }
    }
    pub(crate) fn next_deadline(&self) -> Instant {
        self.keepalive.borrow().next_deadline()
    }
    fn flush(&self) -> Result<()> {
        let Some(reactor) = &self.reactor else {
            return Ok(());
        };
        reactor.check()?;
        if reactor.writable() {
            let mut io = self.io.borrow_mut();
            if io.tls.wants_write() {
                let mut bytes = Vec::new();
                io.tls.write_tls(&mut bytes)?;
                drop(io);
                reactor.send(bytes)?;
            }
        }
        Ok(())
    }
    fn pump(&self) -> Result<Vec<u8>> {
        let Some(reactor) = &self.reactor else {
            return self.io.borrow_mut().pump();
        };
        // Drain already-delivered data before reporting EOF, preserving a final BYE.
        let bytes = reactor.take_bytes();
        if bytes.is_empty() {
            reactor.check()?;
        }
        let mut io = self.io.borrow_mut();
        if let Some(error) = &io.failed {
            return Err(Error::Sip(error.clone()));
        }
        let mut cursor = std::io::Cursor::new(bytes);
        while (cursor.position() as usize) < cursor.get_ref().len() {
            io.tls.read_tls(&mut cursor)?;
            io.tls
                .process_new_packets()
                .map_err(|error| Error::Sip(format!("TLS: {error}")))?;
        }
        let mut plaintext = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match io.tls.reader().read(&mut buffer) {
                Ok(0) => break,
                Ok(size) => {
                    plaintext.extend_from_slice(&buffer[..size]);
                    if plaintext.len() > 65536 {
                        return Err(Error::Sip("TLS receive buffer limit exceeded".into()));
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        drop(io);
        if reactor.check().is_ok() {
            self.flush()?;
        }
        Ok(plaintext)
    }
    pub fn poll(&self) -> Result<()> {
        if self.destroyed.get() {
            return Err(Error::Sip("TLS transport closed".into()));
        }
        let expiring = {
            let state = self.keepalive.borrow();
            state.pong_supported
                && state.pending_ping.is_some()
                && Instant::now() >= state.next_deadline()
        };
        if expiring && self.reactor.is_some() {
            // After a worker stall the pong may still be in the OS socket,
            // not yet in our callback queue. Dispatch ready I/O before expiry.
            // PJPROJECT remains the sole reader of the registered descriptor.
            for _ in 0..16 {
                let ready = unsafe {
                    pj::pj_ioqueue_poll(
                        pj::pjsip_endpt_get_ioqueue((*self.base.get()).endpt),
                        &pj::pj_time_val { sec: 0, msec: 0 },
                    )
                };
                if ready <= 0 {
                    break;
                }
            }
        }
        let bytes = self.pump()?;
        self.incoming.borrow_mut().extend(bytes);
        loop {
            {
                let mut incoming = self.incoming.borrow_mut();
                if incoming.starts_with(b"\r\n") {
                    incoming.drain(..2);
                    if self.keepalive.borrow_mut().crlf() {
                        self.io.borrow_mut().tls.writer().write_all(b"\r\n")?;
                        self.flush()?;
                    }
                    continue;
                }
                if incoming.as_slice() == b"\r" {
                    break;
                }
                if !incoming.is_empty() {
                    self.keepalive.borrow_mut().message();
                }
            }
            let mut received = Box::<pj::pjsip_rx_data>::default();
            let size = {
                let incoming = self.incoming.borrow();
                let size = incoming.len().min(received.pkt_info.packet.len() - 1);
                for (target, byte) in received.pkt_info.packet.iter_mut().zip(&incoming[..size]) {
                    *target = *byte as _;
                }
                size
            };
            if size == 0 {
                break;
            }
            unsafe {
                received.tp_info.pool = self.receive_pool;
                received.tp_info.transport = self.base.get();
                received.pkt_info.len = size as _;
                received.pkt_info.src_addr = (*self.base.get()).key.rem_addr;
                received.pkt_info.src_addr_len = (*self.base.get()).addr_len;
                received.pkt_info.src_port =
                    pj::pj_sockaddr_get_port(ptr::from_ref(&received.pkt_info.src_addr).cast())
                        as _;
                pj::pj_sockaddr_print(
                    ptr::from_ref(&received.pkt_info.src_addr).cast(),
                    received.pkt_info.src_name.as_mut_ptr(),
                    received.pkt_info.src_name.len() as _,
                    0,
                );
                pj::pj_gettimeofday(&mut received.pkt_info.timestamp);
                let consumed =
                    pj::pjsip_tpmgr_receive_packet((*self.base.get()).tpmgr, received.as_mut());
                pj::pj_pool_reset(self.receive_pool);
                match consumed {
                    1.. => {
                        self.incoming.borrow_mut().drain(..consumed as usize);
                    }
                    _ if size == received.pkt_info.packet.len() - 1 => {
                        return Err(Error::Sip(
                            "TLS SIP message exceeds PJSIP receive limit".into(),
                        ));
                    }
                    _ => break,
                }
            }
        }
        if self.keepalive.borrow_mut().tick(Instant::now())? {
            self.io.borrow_mut().tls.writer().write_all(b"\r\n\r\n")?;
            self.flush()?;
        }
        Ok(())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // Sip drops this owner after destroying the endpoint and before its pool factory.
        self.close_io();
        self.reactor.take();
        let base = self.base.get_mut();
        unsafe {
            if !base.lock.is_null() {
                pj::pj_lock_destroy(base.lock);
            }
            if !base.ref_cnt.is_null() {
                pj::pj_atomic_destroy(base.ref_cnt);
            }
            if !self.receive_pool.is_null() {
                pj::pj_pool_release(self.receive_pool);
            }
            pj::pj_pool_release(base.pool);
        }
    }
}

unsafe fn address(address: SocketAddr) -> Result<pj::pj_sockaddr> {
    let mut native = pj::pj_sockaddr::default();
    unsafe {
        check(pj::pj_sockaddr_init(
            pj::PJ_AF_INET as _,
            &mut native,
            &pjstr(&address.ip().to_string()),
            address.port(),
        ))?;
    }
    Ok(native)
}

unsafe extern "C" fn send(
    base: *mut pj::pjsip_transport,
    data: *mut pj::pjsip_tx_data,
    _: *const pj::pj_sockaddr_t,
    _: i32,
    _: *mut std::ffi::c_void,
    _: pj::pjsip_transport_callback,
) -> pj::pj_status_t {
    unsafe {
        let owner = &*base.cast::<Transport>();
        let bytes = std::slice::from_raw_parts(
            (*data).buf.start.cast(),
            (*data).buf.cur.offset_from((*data).buf.start) as usize,
        );
        let mut io = owner.io.borrow_mut();
        match io.tls.writer().write_all(bytes) {
            Ok(()) => {
                drop(io);
                match owner.flush() {
                    Ok(()) => 0,
                    Err(error) => {
                        owner.io.borrow_mut().failed = Some(error.to_string());
                        pj::PJSIP_TLS_EUNKNOWN as _
                    }
                }
            }
            Err(error) => {
                io.failed = Some(format!("TLS send failed: {error}"));
                pj::PJSIP_TLS_EUNKNOWN as _
            }
        }
    }
}
unsafe extern "C" fn shutdown(base: *mut pj::pjsip_transport) -> pj::pj_status_t {
    unsafe {
        (&*base.cast::<Transport>())
            .io
            .borrow_mut()
            .tls
            .send_close_notify();
    }
    0
}
unsafe extern "C" fn destroy(base: *mut pj::pjsip_transport) -> pj::pj_status_t {
    unsafe {
        (&*base.cast::<Transport>()).destroyed.set(true);
    }
    0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
    use std::{net::TcpListener, path::Path, thread};

    thread_local! {
        pub(crate) static TRUST: RefCell<Option<Arc<ClientConfig>>> = const { RefCell::new(None) };
    }

    #[test]
    fn keepalive_does_not_require_pongs_until_peer_demonstrates_support() {
        let now = Instant::now();
        let mut state = Keepalive::new(now);
        state.crlf();
        state.message(); // Unsolicited whitespace is not a response to a probe.
        assert!(!state.pong_supported);
        assert!(state.tick(now + PING_INTERVAL).unwrap());
        assert_eq!(state.next_deadline(), now + PING_INTERVAL * 2);
        assert!(!state.tick(now + PING_INTERVAL + PONG_TIMEOUT).unwrap());
        assert!(state.tick(now + PING_INTERVAL * 2).unwrap());
        state.crlf();
        state.message();
        assert!(state.pong_supported);
        assert!(state.tick(now + PING_INTERVAL * 3).unwrap());
        assert_eq!(
            state.next_deadline(),
            now + PING_INTERVAL * 3 + PONG_TIMEOUT
        );
        assert!(state.tick(state.next_deadline()).is_err());
    }

    #[test]
    fn keepalive_queued_pong_wins_over_expired_deadline() {
        let now = Instant::now();
        let mut state = Keepalive::new(now);
        state.tick(now + PING_INTERVAL).unwrap();
        state.crlf();
        state.message();
        state.tick(now + PING_INTERVAL * 2).unwrap();
        let resumed = state.next_deadline() + Duration::from_millis(50);
        state.crlf();
        state.message();
        assert!(!state.tick(resumed).unwrap());
        assert_eq!(state.next_deadline(), now + PING_INTERVAL * 3);
    }

    #[test]
    fn fragmented_peer_ping_does_not_establish_pong_support() {
        let now = Instant::now();
        let mut state = Keepalive::new(now);
        state.tick(now + PING_INTERVAL).unwrap();
        assert!(!state.crlf());
        assert!(state.crlf()); // Reply to the completed peer ping.
        assert!(!state.pong_supported);
        assert_eq!(state.pending_ping, Some(now + PING_INTERVAL));
        assert!(!state.tick(now + PING_INTERVAL + PONG_TIMEOUT).unwrap());
    }

    #[test]
    fn whitespace_before_probe_cannot_satisfy_that_probe() {
        let now = Instant::now();
        let mut state = Keepalive::new(now);
        assert!(!state.crlf());
        state.tick(now + PING_INTERVAL).unwrap();
        assert!(state.crlf());
        assert!(!state.pong_supported);
        assert_eq!(state.pending_ping, Some(now + PING_INTERVAL));
    }

    #[test]
    fn pongs_to_separate_probes_are_not_mistaken_for_a_peer_ping() {
        let now = Instant::now();
        let mut state = Keepalive::new(now);
        for round in 1..=3 {
            assert!(state.tick(now + PING_INTERVAL * round).unwrap());
            assert!(!state.crlf());
            assert!(state.pong_supported);
            assert_eq!(state.pending_ping, None);
        }
    }

    fn header<'a>(message: &'a str, name: &str) -> &'a str {
        message
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .unwrap()
            .1
            .trim()
    }

    fn read_message(stream: &mut impl Read) -> std::io::Result<String> {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            bytes.push(byte[0]);
            assert!(bytes.len() < 65536);
        }
        let headers = String::from_utf8(bytes).unwrap();
        let length = header(&headers, "Content-Length").parse::<usize>().unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body)?;
        Ok(headers + &String::from_utf8(body).unwrap())
    }

    fn response(request: &str, status: &str) -> String {
        format!(
            "SIP/2.0 {status}\r\nVia: {}\r\nFrom: {}\r\nTo: {};tag=tls-fixture\r\nCall-ID: {}\r\nCSeq: {}\r\nContent-Length: 0\r\n\r\n",
            header(request, "Via"),
            header(request, "From"),
            header(request, "To"),
            header(request, "Call-ID"),
            header(request, "CSeq")
        )
    }

    #[test]
    fn verified_tls_carries_sip_and_rejects_untrusted_or_wrong_name_certificates() {
        for (name, trusted, proxy, secure_uri) in [
            ("localhost", true, false, false),
            ("localhost", true, true, false),
            ("localhost", true, false, true),
            ("localhost", false, false, false),
            ("wrong.invalid", true, false, false),
        ] {
            let certificate = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let server_config = ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![certificate.cert.der().clone()],
                    PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
                )
                .unwrap();
            let mut roots = RootCertStore::empty();
            if trusted {
                roots.add(certificate.cert.der().clone()).unwrap();
            }
            let config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            TRUST.with(|trust| *trust.borrow_mut() = Some(Arc::new(config)));
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut stream = StreamOwned::new(
                    ServerConnection::new(Arc::new(server_config)).unwrap(),
                    socket,
                );
                let registration = match read_message(&mut stream) {
                    Ok(message) => message,
                    Err(_) => return false,
                };
                assert!(registration.starts_with("REGISTER "), "{registration}");
                assert!(header(&registration, "Via").starts_with("SIP/2.0/TLS"));
                let contact = header(&registration, "Contact").trim_matches(['<', '>']);
                let options = format!(
                    "OPTIONS {contact} SIP/2.0\r\nVia: SIP/2.0/TLS 127.0.0.1:{port};branch=z9hG4bKtls-probe\r\nFrom: <sip:probe@localhost>;tag=probe\r\nTo: <{contact}>\r\nCall-ID: tls-probe\r\nCSeq: 1 OPTIONS\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
                );
                let split = options.len() / 2;
                stream.write_all(&options.as_bytes()[..split]).unwrap();
                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(20));
                stream.write_all(&options.as_bytes()[split..]).unwrap();
                stream.flush().unwrap();
                let answer = read_message(&mut stream).unwrap();
                assert!(answer.starts_with("SIP/2.0 200"), "{answer}");
                assert_eq!(header(&answer, "CSeq"), "1 OPTIONS");
                stream
                    .write_all(response(&registration, "200 OK").as_bytes())
                    .unwrap();
                stream.flush().unwrap();
                let invite = read_message(&mut stream).unwrap();
                assert!(invite.starts_with("INVITE "), "{invite}");
                assert!(header(&invite, "Via").starts_with("SIP/2.0/TLS"));
                stream
                    .write_all(response(&invite, "486 Busy Here").as_bytes())
                    .unwrap();
                stream.flush().unwrap();
                while let Ok(message) = read_message(&mut stream) {
                    if message.starts_with("REGISTER ") {
                        stream
                            .write_all(response(&message, "200 OK").as_bytes())
                            .unwrap();
                        stream.flush().unwrap();
                        break;
                    }
                    assert!(message.starts_with("ACK "), "{message}");
                }
                true
            });
            let proxy_uri = format!("sip:localhost:{port};lr");
            let destination = match secure_uri {
                true => format!("sips:3003@localhost:{port}"),
                false => "3003".into(),
            };
            let result = crate::send(
                crate::SendRequest {
                    account: crate::Account {
                        server: "localhost",
                        port,
                        transport: crate::SignalingTransport::Tls,
                        username: "fixture",
                        auth_username: None,
                        password: None,
                        register: true,
                        outbound_proxy: proxy.then_some(proxy_uri.as_str()),
                        audio_playout_delay_ms: 200,
                    },
                    destination: &destination,
                    file: Path::new("unused-before-media.tiff"),
                    station_id: "FAXE TEST",
                    mode: crate::Mode::Auto,
                },
                || false,
                |_| {},
            );
            TRUST.with(|trust| trust.borrow_mut().take());
            let error = result.unwrap_err().to_string();
            match trusted && name == "localhost" {
                true => {
                    assert!(server.join().unwrap());
                    assert!(error.contains("486"), "{error}");
                }
                false => {
                    assert!(!server.join().unwrap());
                    assert!(error.contains("certificate"), "{error}");
                }
            }
        }
    }
    #[test]
    fn persistent_tls_answers_inbound_and_fragmented_crlf_then_preserves_bye_reason() {
        receive_fixture(Duration::ZERO);
    }

    #[test]
    fn persistent_tls_without_pong_support_survives_old_35_second_cutoff() {
        receive_fixture(Duration::from_secs(36));
    }

    /// Run alone with --release --ignored --nocapture, then sample the printed PID.
    #[test]
    #[ignore = "manual 20-second idle profiling fixture"]
    fn profile_idle_tls() {
        receive_fixture(Duration::from_secs(20));
    }

    fn receive_fixture(idle: Duration) {
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
            )
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(certificate.cert.der().clone()).unwrap();
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        TRUST.with(|slot| *slot.borrow_mut() = Some(Arc::new(config)));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (service, events) = crate::SipService::start().unwrap();
        TRUST.with(|slot| slot.borrow_mut().take());
        let profile = uuid::Uuid::new_v4();
        let spool = std::env::temp_dir().join(format!("faxe-tls-receive-{profile}.tiff"));
        service
            .configure(Some(crate::ReceiveConfig {
                account: crate::ServiceAccount {
                    id: profile,
                    server: "localhost".into(),
                    port,
                    transport: crate::SignalingTransport::Tls,
                    username: "inbox".into(),
                    auth_username: None,
                    password: None,
                    register: true,
                    outbound_proxy: None,
                    audio_playout_delay_ms: 200,
                    automatic_nat: true,
                    stun_server: None,
                },
                station_id: "FIXTURE".into(),
                mode: crate::Mode::T38,
                ecm: true,
                offer: Arc::new(move |request| {
                    let _ = request.respond(Ok(spool.clone()));
                }),
            }))
            .unwrap();
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut stream = StreamOwned::new(
            ServerConnection::new(Arc::new(server_config)).unwrap(),
            socket,
        );
        let registration = read_message(&mut stream).unwrap();
        stream
            .write_all(response(&registration, "200 OK").as_bytes())
            .unwrap();
        stream.flush().unwrap();
        loop {
            if matches!(
                events.recv_timeout(Duration::from_secs(5)).unwrap(),
                crate::ReceiveEvent::Status {
                    state: crate::ReceiveState::Ready,
                    ..
                }
            ) {
                break;
            }
        }
        if !idle.is_zero() {
            eprintln!("TLS ready and idle: PID {}", std::process::id());
            thread::sleep(idle);
            assert!(
                events.try_recv().is_err(),
                "idle SIP published a state change"
            );
        }
        if idle >= PING_INTERVAL {
            let mut probe = [0; 4];
            stream.read_exact(&mut probe).unwrap();
            assert_eq!(&probe, b"\r\n\r\n");
            // Intentionally never pong. An incoming ping doesn't demonstrate
            // support for answering our probes, either.
            stream.write_all(b"\r\n\r\n").unwrap();
            stream.flush().unwrap();
        } else {
            stream.write_all(b"\r\n").unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(30));
            stream.write_all(b"\r\n").unwrap();
            stream.flush().unwrap();
        }
        let mut pong = [0; 2];
        stream.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"\r\n");
        let contact = header(&registration, "Contact").trim_matches(['<', '>']);
        let body = "v=0\r\no=fixture 1 1 IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=image 45000 udptl t38\r\na=T38FaxVersion:0\r\na=T38FaxRateManagement:transferredTCF\r\na=T38FaxUdpEC:t38UDPRedundancy\r\n";
        let invite = format!(
            "INVITE {contact} SIP/2.0\r\nVia: SIP/2.0/TLS 127.0.0.1:{port};branch=z9hG4bKinbound\r\nFrom: <sip:caller@localhost>;tag=caller\r\nTo: <{contact}>\r\nCall-ID: inbound-tls\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContact: <sip:caller@localhost:{port};transport=tls>\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(invite.as_bytes()).unwrap();
        stream.flush().unwrap();
        let provisional = read_message(&mut stream).unwrap();
        assert!(provisional.starts_with("SIP/2.0 100"), "{provisional}");
        let answer = read_message(&mut stream).unwrap();
        assert!(answer.starts_with("SIP/2.0 200"), "{answer}");
        let to = header(&answer, "To");
        for (method, sequence, reason) in [
            ("ACK", 1, ""),
            (
                "BYE",
                2,
                "Reason: SIP;cause=408;text=\"Fixture interrupted\"\r\n",
            ),
        ] {
            let request = format!(
                "{method} {contact} SIP/2.0\r\nVia: SIP/2.0/TLS 127.0.0.1:{port};branch=z9hG4bKinbound-{method}\r\nFrom: <sip:caller@localhost>;tag=caller\r\nTo: {to}\r\nCall-ID: inbound-tls\r\nCSeq: {sequence} {method}\r\nMax-Forwards: 70\r\n{reason}Content-Length: 0\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
        let bye_answer = read_message(&mut stream).unwrap();
        assert!(bye_answer.starts_with("SIP/2.0 200"), "{bye_answer}");
        loop {
            if let crate::ReceiveEvent::Finished { outcome, .. } =
                events.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                assert!(
                    outcome
                        .unwrap_err()
                        .to_string()
                        .contains("Fixture interrupted")
                );
                break;
            }
        }
        service.shutdown();
    }
}
