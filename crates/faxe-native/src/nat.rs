//! IPv4 STUN discovery on the media socket itself; no relay allocation.
use crate::{Error, Result};
use std::{
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};
const COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
pub(crate) struct Mapping {
    pub server: SocketAddr,
    transaction: [u8; 12],
    pub address: Option<SocketAddr>,
    started: Instant,
    sent: Instant,
    attempts: u8,
}
impl Mapping {
    pub fn new(server: SocketAddr) -> Self {
        Self {
            server,
            transaction: uuid::Uuid::new_v4().as_bytes()[..12].try_into().unwrap(),
            address: None,
            started: Instant::now(),
            sent: Instant::now() - Duration::from_secs(30),
            attempts: 0,
        }
    }
    pub fn pending(&self) -> bool {
        self.address.is_none() && self.started.elapsed() < Duration::from_secs(3)
    }
    pub fn next_deadline(&self) -> Instant {
        let interval = if self.attempts < 3 && self.address.is_none() {
            Duration::from_millis(500 * (1 << self.attempts))
        } else {
            Duration::from_secs(15)
        };
        let next = self.sent + interval;
        if self.pending() {
            next.min(self.started + Duration::from_secs(3))
        } else {
            next
        }
    }
    pub fn tick(&mut self, socket: &crate::sip::io::DatagramSocket) -> Result<()> {
        let interval = if self.attempts < 3 && self.address.is_none() {
            Duration::from_millis(500 * (1 << self.attempts))
        } else {
            Duration::from_secs(15)
        };
        if self.sent.elapsed() >= interval {
            if self.attempts >= 3 || self.address.is_some() {
                self.transaction = uuid::Uuid::new_v4().as_bytes()[..12].try_into().unwrap();
            }
            let mut request = [0_u8; 20];
            request[..2].copy_from_slice(&[0, 1]);
            request[4..8].copy_from_slice(&COOKIE);
            request[8..20].copy_from_slice(&self.transaction);
            socket.send_to(&request, self.server)?;
            self.sent = Instant::now();
            self.attempts = self.attempts.saturating_add(1);
        }
        Ok(())
    }
    pub fn receive(&mut self, source: SocketAddr, packet: &[u8]) -> bool {
        if source != self.server || !is_stun(packet) {
            return false;
        }
        if let Some(address) = parse_response(packet, self.transaction) {
            self.address = Some(address);
        }
        true
    }
}
pub(crate) fn is_stun(packet: &[u8]) -> bool {
    packet.len() >= 20 && packet[0] & 0xc0 == 0 && packet[4..8] == COOKIE
}
fn parse_response(packet: &[u8], transaction: [u8; 12]) -> Option<SocketAddr> {
    if !is_stun(packet) || packet[..2] != [1, 1] || packet[8..20] != transaction {
        return None;
    }
    let length = u16::from_be_bytes(packet[2..4].try_into().ok()?) as usize;
    if !length.is_multiple_of(4) || packet.len() != 20 + length {
        return None;
    }
    let mut offset = 20;
    let mut mapped = None;
    while offset + 4 <= packet.len() {
        let kind = u16::from_be_bytes(packet[offset..offset + 2].try_into().ok()?);
        let length = u16::from_be_bytes(packet[offset + 2..offset + 4].try_into().ok()?) as usize;
        let value = packet.get(offset + 4..offset + 4 + length)?;
        if matches!(kind, 0x0001 | 0x0020) && value.len() == 8 && value[1] == 1 {
            let mut port = u16::from_be_bytes(value[2..4].try_into().ok()?);
            let mut ip: [u8; 4] = value[4..8].try_into().ok()?;
            if kind == 0x0020 {
                port ^= 0x2112;
                for i in 0..4 {
                    ip[i] ^= COOKIE[i];
                }
            }
            let ip = Ipv4Addr::from(ip);
            if port == 0 || ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
                return None;
            }
            mapped = Some(SocketAddr::from((ip, port)));
            if kind == 0x0020 {
                return mapped;
            }
        }
        offset += 4 + length.div_ceil(4) * 4;
    }
    mapped
}

/// Learn only a port on the SDP peer's IP, after sequential valid packets.
/// Once selected, pin the peer for the call. This supports PBX/SBC media relays
/// without accepting arbitrary endpoints that merely know our UDP port.
pub(crate) struct Peer {
    pub address: SocketAddr,
    enabled: bool,
    pinned: bool,
    candidate: Option<(SocketAddr, u16, u32, u8)>,
}
impl Peer {
    pub fn new(address: SocketAddr, enabled: bool) -> Self {
        Self {
            address,
            enabled,
            pinned: false,
            candidate: None,
        }
    }
    pub fn accept(&mut self, source: SocketAddr, sequence: u16, stream: u32) -> bool {
        if source == self.address {
            self.pinned = true;
            return true;
        }
        if !self.enabled || self.pinned || source.ip() != self.address.ip() {
            return false;
        }
        let count = match self.candidate {
            Some((peer, previous, id, count))
                if peer == source && id == stream && sequence == previous.wrapping_add(1) =>
            {
                count + 1
            }
            _ => 1,
        };
        self.candidate = Some((source, sequence, stream, count));
        if count < 3 {
            return false;
        }
        self.address = source;
        self.pinned = true;
        true
    }
}

pub(crate) fn stun_server(value: &str) -> Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    let value = value.strip_prefix("stun:").unwrap_or(value);
    let address = if value.contains(':') {
        value.to_owned()
    } else {
        format!("{value}:3478")
    };
    address
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| Error::Invalid("No IPv4 STUN server address".into()))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    #[test]
    fn stun_validates_transaction_and_decodes_xor_mapping() {
        let id = [9; 12];
        let mut packet = vec![1, 1, 0, 12];
        packet.extend(COOKIE);
        packet.extend(id);
        packet.extend([0, 0x20, 0, 8, 0, 1]);
        packet.extend((42000_u16 ^ 0x2112).to_be_bytes());
        packet.extend([203 ^ COOKIE[0], COOKIE[1], 113 ^ COOKIE[2], 5 ^ COOKIE[3]]);
        assert_eq!(
            parse_response(&packet, id),
            Some("203.0.113.5:42000".parse().unwrap())
        );
        assert_eq!(parse_response(&packet, [8; 12]), None);
        packet.truncate(27);
        assert_eq!(parse_response(&packet, id), None);
    }
    #[test]
    fn symmetric_peer_requires_valid_sequence_and_pins_the_relay() {
        let mut peer = Peer::new("192.0.2.1:1000".parse().unwrap(), true);
        let translated = "192.0.2.1:2000".parse().unwrap();
        assert!(!peer.accept("192.0.2.2:2000".parse().unwrap(), 1, 42));
        assert!(!peer.accept(translated, 1, 42));
        assert!(!peer.accept(translated, 2, 42));
        assert!(peer.accept(translated, 3, 42));
        assert!(!peer.accept("192.0.2.1:3000".parse().unwrap(), 4, 42));
    }
    #[test]
    fn discovers_and_refreshes_mappings_on_the_actual_media_sockets() -> Result<()> {
        let server = UdpSocket::bind("127.0.0.1:0")?;
        server.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut sockets = crate::network::Sockets::bind("127.0.0.1".parse().unwrap())?;
        sockets.configure_nat(true, Some(server.local_addr()?))?;
        let expected = [sockets.audio.local_addr()?, sockets.fax.local_addr()?];
        let mut peers = Vec::new();
        let mut bytes = [0_u8; 256];
        for index in 0..2 {
            let (size, source) = server.recv_from(&mut bytes)?;
            assert_eq!(size, 20);
            assert!(expected.contains(&source));
            peers.push(source);
            let mut response = vec![1, 1, 0, 12];
            response.extend(COOKIE);
            response.extend(&bytes[8..20]);
            response.extend([0, 0x20, 0, 8, 0, 1]);
            response.extend(((42000 + index) as u16 ^ 0x2112).to_be_bytes());
            response.extend([203 ^ COOKIE[0], COOKIE[1], 113 ^ COOKIE[2], 5 ^ COOKIE[3]]);
            server.send_to(&response, source)?;
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while sockets.mapping_pending() {
            sockets.poll_mappings()?;
            assert!(Instant::now() < deadline, "STUN responses did not arrive");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            sockets
                .audio_mapping
                .as_ref()
                .unwrap()
                .borrow()
                .address
                .unwrap()
                .ip(),
            "203.0.113.5".parse::<std::net::IpAddr>().unwrap()
        );
        for mapping in [&sockets.audio_mapping, &sockets.fax_mapping]
            .into_iter()
            .flatten()
        {
            mapping.borrow_mut().sent = Instant::now() - Duration::from_secs(16);
        }
        sockets.poll_mappings()?;
        for _ in 0..2 {
            let (_, peer) = server.recv_from(&mut bytes)?;
            assert!(peers.contains(&peer));
        }
        Ok(())
    }
}
