//! A loopback doorbell registered in PJPROJECT's own I/O queue.
//! It carries no commands or data: producers enqueue first, then ring it.
use super::*;
use std::{net::UdpSocket, sync::Arc};

#[derive(Clone)]
pub(super) struct Wake(Arc<UdpSocket>);
impl Wake {
    pub fn pair() -> Result<(Self, UdpSocket)> {
        let reader = UdpSocket::bind("127.0.0.1:0")?;
        let writer = UdpSocket::bind("127.0.0.1:0")?;
        writer.connect(reader.local_addr()?)?;
        writer.set_nonblocking(true)?;
        Ok((Self(Arc::new(writer)), reader))
    }
    pub fn notify(&self) {
        if let Err(error) = self.0.send(&[1])
            && error.kind() != std::io::ErrorKind::WouldBlock
        {
            tracing::warn!(%error,"SIP wake notification failed");
        }
    }
}
pub(super) struct Registration {
    socket: *mut pj::pj_activesock_t,
    pool: *mut pj::pj_pool_t,
    _endpoint: Rc<Endpoint>,
}
impl Registration {
    pub fn new(endpoint: Rc<Endpoint>, reader: UdpSocket) -> Result<Self> {
        unsafe {
            let pool =
                pj::pjsip_endpt_create_pool(endpoint.endpt, c"faxe-wake".as_ptr(), 1024, 1024);
            if pool.is_null() {
                return Err(Error::Sip("Cannot allocate wake socket pool".into()));
            }
            let mut registration = Self {
                socket: ptr::null_mut(),
                pool,
                _endpoint: endpoint.clone(),
            };
            let callbacks = pj::pj_activesock_cb {
                on_data_recvfrom: Some(received),
                ..Default::default()
            };
            #[cfg(unix)]
            let socket = {
                use std::os::fd::IntoRawFd;
                reader.into_raw_fd() as pj::pj_sock_t
            };
            #[cfg(windows)]
            let socket = {
                use std::os::windows::io::IntoRawSocket;
                reader.into_raw_socket() as pj::pj_sock_t
            };
            let status = pj::pj_activesock_create(
                pool,
                socket,
                pj::PJ_SOCK_DGRAM as _,
                ptr::null(),
                pj::pjsip_endpt_get_ioqueue(endpoint.endpt),
                &callbacks,
                ptr::null_mut(),
                &mut registration.socket,
            );
            if status != 0 {
                pj::pj_sock_close(socket);
                check(status)?;
            }
            check(pj::pj_activesock_start_recvfrom(
                registration.socket,
                pool,
                128,
                0,
            ))?;
            Ok(registration)
        }
    }
}
unsafe extern "C" fn received(
    _: *mut pj::pj_activesock_t,
    _: *mut std::ffi::c_void,
    _: pj::pj_size_t,
    _: *const pj::pj_sockaddr_t,
    _: i32,
    _: pj::pj_status_t,
) -> pj::pj_bool_t {
    1
}
impl Drop for Registration {
    fn drop(&mut self) {
        unsafe {
            if !self.socket.is_null() {
                pj::pj_activesock_close(self.socket);
            }
            pj::pj_pool_release(self.pool);
        }
    }
}
