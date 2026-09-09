//! Buffered native I/O. PJPROJECT is the sole reader of registered descriptors.
use super::*;
use std::{
    ffi::c_void,
    net::{SocketAddr, TcpStream, UdpSocket},
};

struct Write {
    key: pj::pj_ioqueue_op_key_t,
    bytes: Vec<u8>,
    pending: bool,
}
struct State {
    packets: RefCell<VecDeque<(Vec<u8>, SocketAddr)>>,
    bytes: RefCell<Vec<u8>>,
    error: Cell<Option<pj::pj_status_t>>,
    write: RefCell<Write>,
}
pub(crate) struct Io {
    active: Cell<*mut pj::pj_activesock_t>,
    pool: *mut pj::pj_pool_t,
    state: Box<State>,
}
impl Io {
    unsafe fn new(
        endpoint: *mut pj::pjsip_endpoint,
        socket: pj::pj_sock_t,
        stream: bool,
    ) -> Result<Self> {
        unsafe {
            let pool = pj::pjsip_endpt_create_pool(endpoint, c"faxe-io".as_ptr(), 4096, 4096);
            if pool.is_null() {
                pj::pj_sock_close(socket);
                return Err(Error::Sip("Cannot allocate native I/O pool".into()));
            }
            let mut io = Self {
                active: Cell::new(ptr::null_mut()),
                pool,
                state: Box::new(State {
                    packets: RefCell::new(VecDeque::new()),
                    bytes: RefCell::new(Vec::new()),
                    error: Cell::new(None),
                    write: RefCell::new(Write {
                        key: std::mem::zeroed(),
                        bytes: Vec::new(),
                        pending: false,
                    }),
                }),
            };
            let cb = pj::pj_activesock_cb {
                on_data_read: Some(read),
                on_data_recvfrom: Some(datagram),
                on_data_sent: Some(sent),
                ..Default::default()
            };
            let mut config: pj::pj_activesock_cfg = std::mem::zeroed();
            pj::pj_activesock_cfg_default(&mut config);
            config.concurrency = 0;
            let mut active = ptr::null_mut();
            let status = pj::pj_activesock_create(
                pool,
                socket,
                if stream {
                    pj::PJ_SOCK_STREAM as _
                } else {
                    pj::PJ_SOCK_DGRAM as _
                },
                &config,
                pj::pjsip_endpt_get_ioqueue(endpoint),
                &cb,
                ptr::from_mut(io.state.as_mut()).cast(),
                &mut active,
            );
            if status != 0 {
                pj::pj_sock_close(socket);
                check(status)?;
            }
            io.active.set(active);
            if stream {
                check(pj::pj_activesock_start_read(active, pool, 16384, 0))?;
            } else {
                check(pj::pj_activesock_start_recvfrom(active, pool, 65536, 0))?;
            }
            Ok(io)
        }
    }
    pub(crate) unsafe fn stream(
        endpoint: *mut pj::pjsip_endpoint,
        socket: &TcpStream,
    ) -> Result<Self> {
        let duplicate = socket.try_clone()?;
        #[cfg(unix)]
        let raw = {
            use std::os::fd::IntoRawFd;
            duplicate.into_raw_fd() as pj::pj_sock_t
        };
        #[cfg(windows)]
        let raw = {
            use std::os::windows::io::IntoRawSocket;
            duplicate.into_raw_socket() as pj::pj_sock_t
        };
        unsafe { Self::new(endpoint, raw, true) }
    }
    fn udp(endpoint: *mut pj::pjsip_endpoint, socket: &UdpSocket) -> Result<Self> {
        let duplicate = socket.try_clone()?;
        #[cfg(unix)]
        let raw = {
            use std::os::fd::IntoRawFd;
            duplicate.into_raw_fd() as pj::pj_sock_t
        };
        #[cfg(windows)]
        let raw = {
            use std::os::windows::io::IntoRawSocket;
            duplicate.into_raw_socket() as pj::pj_sock_t
        };
        unsafe { Self::new(endpoint, raw, false) }
    }
    pub fn close(&self) {
        let active = self.active.replace(ptr::null_mut());
        if !active.is_null() {
            unsafe {
                pj::pj_activesock_close(active);
            }
        }
    }
    pub fn check(&self) -> Result<()> {
        if let Some(status) = self.state.error.get() {
            return Err(Error::Sip(format!("Native socket closed: {status}")));
        }
        Ok(())
    }
    pub fn take_bytes(&self) -> Vec<u8> {
        std::mem::take(&mut *self.state.bytes.borrow_mut())
    }
    pub fn writable(&self) -> bool {
        !self.state.write.borrow().pending
    }
    pub fn send(&self, bytes: Vec<u8>) -> Result<()> {
        self.check()?;
        let mut write = self.state.write.borrow_mut();
        if write.pending {
            return Err(Error::Sip("TLS write already pending".into()));
        }
        write.bytes = bytes;
        if write.bytes.is_empty() {
            return Ok(());
        }
        unsafe {
            pj::pj_ioqueue_op_key_init(
                &mut write.key,
                std::mem::size_of::<pj::pj_ioqueue_op_key_t>(),
            );
            let mut size = write.bytes.len() as pj::pj_ssize_t;
            let data = write.bytes.as_ptr();
            let status = pj::pj_activesock_send(
                self.active.get(),
                &mut write.key,
                data.cast(),
                &mut size,
                0,
            );
            if status == 70002 {
                write.pending = true;
            } else {
                check(status)?;
                write.bytes.clear();
            }
        }
        Ok(())
    }
}
impl Drop for Io {
    fn drop(&mut self) {
        self.close();
        unsafe {
            pj::pj_pool_release(self.pool);
        }
    }
}
unsafe fn state<'a>(socket: *mut pj::pj_activesock_t) -> &'a State {
    unsafe { &*pj::pj_activesock_get_user_data(socket).cast::<State>() }
}
unsafe extern "C" fn read(
    socket: *mut pj::pj_activesock_t,
    data: *mut c_void,
    size: pj::pj_size_t,
    status: pj::pj_status_t,
    remainder: *mut pj::pj_size_t,
) -> pj::pj_bool_t {
    unsafe {
        let state = state(socket);
        *remainder = 0;
        if status != 0 || size == 0 {
            state.error.set(Some(status));
            return 0;
        }
        let mut bytes = state.bytes.borrow_mut();
        if bytes.len() + size > 1024 * 1024 {
            state.error.set(Some(-1));
            return 0;
        }
        bytes.extend_from_slice(std::slice::from_raw_parts(data.cast::<u8>(), size));
        1
    }
}
unsafe extern "C" fn datagram(
    socket: *mut pj::pj_activesock_t,
    data: *mut c_void,
    size: pj::pj_size_t,
    address: *const pj::pj_sockaddr_t,
    _: i32,
    status: pj::pj_status_t,
) -> pj::pj_bool_t {
    unsafe {
        let state = state(socket);
        if status != 0 {
            state.error.set(Some(status));
            return 0;
        }
        let mut host = [0 as std::ffi::c_char; 64];
        pj::pj_sockaddr_print(address, host.as_mut_ptr(), host.len() as _, 0);
        let ip = std::ffi::CStr::from_ptr(host.as_ptr())
            .to_string_lossy()
            .parse();
        if let Ok(ip) = ip {
            let peer = SocketAddr::new(ip, pj::pj_sockaddr_get_port(address));
            let mut packets = state.packets.borrow_mut();
            if packets.len() == 256 {
                packets.pop_front();
            }
            packets.push_back((
                std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec(),
                peer,
            ));
        }
        1
    }
}
unsafe extern "C" fn sent(
    socket: *mut pj::pj_activesock_t,
    _: *mut pj::pj_ioqueue_op_key_t,
    size: pj::pj_ssize_t,
) -> pj::pj_bool_t {
    unsafe {
        let state = state(socket);
        let mut write = state.write.borrow_mut();
        write.pending = false;
        write.bytes.clear();
        if size < 0 {
            state.error.set(Some((-size) as _));
        }
        1
    }
}

struct DatagramOwner {
    socket: UdpSocket,
    io: Option<Io>,
    _endpoint: Option<Rc<Endpoint>>,
}
#[derive(Clone)]
pub(crate) struct DatagramSocket(Rc<DatagramOwner>);
impl DatagramSocket {
    pub fn new(socket: UdpSocket) -> Result<Self> {
        let endpoint = ENDPOINT.with(|slot| slot.borrow().upgrade());
        let io = endpoint
            .as_ref()
            .map(|endpoint| Io::udp(endpoint.endpt, &socket))
            .transpose()?;
        Ok(Self(Rc::new(DatagramOwner {
            socket,
            io,
            _endpoint: endpoint,
        })))
    }
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.0.socket.local_addr()
    }
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(self.clone())
    }
    pub fn send_to(&self, bytes: &[u8], peer: SocketAddr) -> std::io::Result<usize> {
        self.0.socket.send_to(bytes, peer)
    }
    fn receive(&self, buffer: &mut [u8], peek: bool) -> std::io::Result<(usize, SocketAddr)> {
        if let Some(io) = &self.0.io {
            let mut packets = io.state.packets.borrow_mut();
            if let Some((bytes, peer)) = packets.front() {
                let count = bytes.len().min(buffer.len());
                buffer[..count].copy_from_slice(&bytes[..count]);
                let peer = *peer;
                if !peek {
                    packets.pop_front();
                }
                return Ok((count, peer));
            }
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        if peek {
            self.0.socket.peek_from(buffer)
        } else {
            self.0.socket.recv_from(buffer)
        }
    }
    pub fn recv_from(&self, buffer: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.receive(buffer, false)
    }
    pub fn peek_from(&self, buffer: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.receive(buffer, true)
    }
}
