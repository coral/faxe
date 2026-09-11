use crate::sip::io::DatagramSocket;
use crate::{
    Error, FRAME_SAMPLES, G711, IfpPacket, Result,
    nat::{Mapping, Peer},
    playout::{Playout, ReceiveStats},
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    io::ErrorKind,
    net::{IpAddr, SocketAddr, UdpSocket},
    rc::Rc,
    time::{Duration, Instant},
};
use udptl::{ErrorRecovery, RedundancyReceiver, UdptlPacket};

pub(crate) struct Sockets {
    pub audio: DatagramSocket,
    pub _rtcp: DatagramSocket,
    pub fax: DatagramSocket,
    pub automatic_nat: bool,
    pub audio_mapping: Option<Rc<RefCell<Mapping>>>,
    pub fax_mapping: Option<Rc<RefCell<Mapping>>>,
}

impl Sockets {
    pub fn next_deadline(&self) -> Option<Instant> {
        [&self.audio_mapping, &self.fax_mapping]
            .into_iter()
            .flatten()
            .map(|mapping| mapping.borrow().next_deadline())
            .min()
    }
    pub fn configure_nat(
        &mut self,
        automatic: bool,
        server: Option<std::net::SocketAddr>,
    ) -> Result<()> {
        self.automatic_nat = automatic;
        if automatic && let Some(server) = server {
            self.audio_mapping = Some(Rc::new(RefCell::new(Mapping::new(server))));
            self.fax_mapping = Some(Rc::new(RefCell::new(Mapping::new(server))));
            self.poll_mappings()?;
        }
        Ok(())
    }
    pub fn poll_mappings(&self) -> Result<()> {
        for (socket, mapping) in [
            (&self.audio, &self.audio_mapping),
            (&self.fax, &self.fax_mapping),
        ] {
            if let Some(mapping) = mapping {
                let mut mapping = mapping.borrow_mut();
                mapping.tick(socket)?;
                let mut bytes = [0_u8; 2048];
                for _ in 0..16 {
                    match socket.peek_from(&mut bytes) {
                        Ok((size, peer))
                            if peer == mapping.server && crate::nat::is_stun(&bytes[..size]) =>
                        {
                            let (size, peer) = socket.recv_from(&mut bytes)?;
                            mapping.receive(peer, &bytes[..size]);
                        }
                        _ => break,
                    }
                }
            }
        }
        Ok(())
    }
    pub fn mapping_pending(&self) -> bool {
        [&self.audio_mapping, &self.fax_mapping]
            .into_iter()
            .flatten()
            .any(|mapping| mapping.borrow().pending())
    }
    pub fn mapped_local(&self, mut local: crate::sdp::LocalMedia) -> crate::sdp::LocalMedia {
        let audio = self.audio_mapping.as_ref().and_then(|m| m.borrow().address);
        let fax = self.fax_mapping.as_ref().and_then(|m| m.borrow().address);
        if let (Some(audio), Some(fax)) = (audio, fax)
            && audio.ip() == fax.ip()
        {
            local.ip = audio.ip();
            local.audio_port = audio.port();
            local.fax_port = fax.port();
        }
        local
    }
    pub fn bind(ip: IpAddr) -> Result<Self> {
        for _ in 0..100 {
            let audio = UdpSocket::bind((ip, 0))?;
            let port = audio.local_addr()?.port();
            if port % 2 != 0 {
                continue;
            }
            let rtcp = match UdpSocket::bind((ip, port + 1)) {
                Ok(socket) => socket,
                Err(_) => continue,
            };
            let fax = UdpSocket::bind((ip, 0))?;
            for socket in [&audio, &rtcp, &fax] {
                socket.set_nonblocking(true)?;
            }
            return Ok(Self {
                audio: DatagramSocket::new(audio)?,
                _rtcp: DatagramSocket::new(rtcp)?,
                fax: DatagramSocket::new(fax)?,
                automatic_nat: false,
                audio_mapping: None,
                fax_mapping: None,
            });
        }
        Err(Error::Sip(
            "Could not allocate an RTP/RTCP port pair".into(),
        ))
    }
}

pub(crate) struct AudioNetwork {
    socket: DatagramSocket,
    peer: Peer,
    signaled_address: SocketAddr,
    symmetric: bool,
    mapping: Option<Rc<RefCell<Mapping>>>,
    codec: G711,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    source: Option<u32>,
    source_change_pending: bool,
    retired_source: Option<u32>,
    candidate: Vec<PendingAudio>,
    received_packets: u64,
    malformed_packets: u64,
    unexpected_payload_packets: u64,
    unexpected_peer_packets: u64,
    unselected_source_packets: u64,
    playout: Playout,
    reported: ReceiveStats,
    last_report: Instant,
    pub last_received: Instant,
}

struct PendingAudio {
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    payload: Vec<u8>,
}

impl AudioNetwork {
    pub fn new(
        socket: &DatagramSocket,
        address: SocketAddr,
        codec: G711,
        symmetric: bool,
        mapping: Option<Rc<RefCell<Mapping>>>,
        playout_delay_ms: u16,
    ) -> Result<Self> {
        let socket = socket.try_clone()?;

        let random = *uuid::Uuid::new_v4().as_bytes();
        tracing::debug!(peer = %address, ?codec, playout_delay_ms, "RTP socket connected");
        Ok(Self {
            socket,
            peer: Peer::new(address, symmetric),
            signaled_address: address,
            symmetric,
            mapping,
            codec,
            sequence: u16::from_be_bytes([random[0], random[1]]),
            timestamp: u32::from_be_bytes(random[2..6].try_into().unwrap()),
            ssrc: u32::from_be_bytes(random[6..10].try_into().unwrap()),
            source: None,
            source_change_pending: false,
            retired_source: None,
            candidate: Vec::new(),
            received_packets: 0,
            malformed_packets: 0,
            unexpected_payload_packets: 0,
            unexpected_peer_packets: 0,
            unselected_source_packets: 0,
            playout: Playout::new(playout_delay_ms)?,
            reported: ReceiveStats::default(),
            last_report: Instant::now(),
            last_received: Instant::now(),
        })
    }

    pub fn update_remote(&mut self, address: SocketAddr, codec: G711) {
        tracing::info!(peer = %address, ?codec, "Updating G.711 media endpoint");
        if address != self.signaled_address {
            self.signaled_address = address;
            self.peer = Peer::new(address, self.symmetric);
            self.source = None;
            self.source_change_pending = false;
            self.retired_source = None;
            self.candidate.clear();
            self.last_received = Instant::now();
        } else if codec != self.codec {
            // A peer can restart RTP with a new SSRC when renegotiating the
            // codec without changing its address/port. Select the stream on
            // the first valid packet using the new codec from the accepted peer.
            self.source_change_pending = true;
            self.candidate.clear();
        }
        // Keep the fax modem and outgoing RTP clock running across re-INVITEs.
        // Retain decoded samples even when the peer replaces the RTP source.
        self.codec = codec;
    }

    pub fn send(&mut self, samples: [i16; FRAME_SAMPLES]) -> Result<()> {
        let mut packet = [0_u8; 12 + FRAME_SAMPLES];
        packet[0] = 0x80;
        packet[1] = self.codec.payload_type();
        packet[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        packet[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        packet[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        packet[12..].copy_from_slice(&self.codec.encode(samples));
        self.socket.send_to(&packet, self.peer.address)?;
        tracing::trace!(
            direction = "TX",
            sequence = self.sequence,
            timestamp = self.timestamp,
            ssrc = self.ssrc,
            bytes = packet.len(),
            "RTP packet"
        );
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES as u32);
        Ok(())
    }

    pub fn receive(&mut self) -> Result<Option<[i16; FRAME_SAMPLES]>> {
        self.receive_packets()?;
        let frame = self.playout.receive(Instant::now());
        if self.last_report.elapsed() >= Duration::from_secs(1) {
            self.report(false);
        }
        Ok(frame)
    }

    pub fn drain(&mut self) -> Result<Vec<[i16; FRAME_SAMPLES]>> {
        self.receive_packets()?;
        Ok(self.playout.drain())
    }

    fn receive_packets(&mut self) -> Result<()> {
        let mut buffer = [0_u8; 4096];
        for _ in 0..64 {
            let (size, source) = match self.socket.recv_from(&mut buffer) {
                Ok(value) => value,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            };
            if let Some(mapping) = &self.mapping
                && mapping.borrow_mut().receive(source, &buffer[..size])
            {
                continue;
            }
            let Some(packet) = Rtp::parse(&buffer[..size]) else {
                self.malformed_packets += 1;
                tracing::trace!(bytes = size, "Discarding malformed RTP packet");
                continue;
            };
            self.received_packets += 1;
            tracing::trace!(
                direction = "RX",
                timestamp = packet.timestamp,
                ssrc = packet.ssrc,
                payload_type = packet.payload_type,
                bytes = size,
                "RTP packet"
            );
            if packet.payload_type != self.codec.payload_type()
                || packet.payload.is_empty()
                || packet.payload.len() > 3200
            {
                self.unexpected_payload_packets += 1;
                continue;
            }
            if !self.peer.accept(source, packet.sequence, packet.ssrc) {
                self.unexpected_peer_packets += 1;
                continue;
            }
            if self.source.is_some_and(|source| source != packet.ssrc) {
                self.unselected_source_packets += 1;
                if self.retired_source == Some(packet.ssrc) {
                    continue;
                }
                if !self.source_change_pending {
                    // An SBC may restart RTP on entering fax passthrough,
                    // including after a rejected T.38 offer with unchanged SDP.
                    // Require three sequential packets from the pinned peer,
                    // with a consistent sample clock, before replacing it.
                    if self.candidate.last().is_some_and(|previous| {
                        previous.ssrc != packet.ssrc
                            || packet.sequence != previous.sequence.wrapping_add(1)
                            || packet.timestamp
                                != previous
                                    .timestamp
                                    .wrapping_add(previous.payload.len() as u32)
                    }) {
                        self.candidate.clear();
                    }
                    self.candidate.push(PendingAudio {
                        ssrc: packet.ssrc,
                        sequence: packet.sequence,
                        timestamp: packet.timestamp,
                        payload: packet.payload.to_vec(),
                    });
                    if self.candidate.len() < 3 {
                        continue;
                    }
                }
                tracing::info!(
                    previous_ssrc = self.source,
                    ssrc = packet.ssrc,
                    "RTP receive stream restarted"
                );
                self.retired_source = self.source;
                let timestamp = self
                    .candidate
                    .first()
                    .map_or(packet.timestamp, |p| p.timestamp);
                self.playout.restart_stream(timestamp);
                // Keep the probation samples: they can contain fax training.
                if !self.candidate.is_empty() {
                    for pending in self.candidate.drain(..) {
                        self.playout.insert(
                            pending.timestamp,
                            &pending.payload,
                            self.codec,
                            Instant::now(),
                        );
                    }
                    self.source = Some(packet.ssrc);
                    self.last_received = Instant::now();
                    continue;
                }
            }
            self.candidate.clear();
            if self.source.is_none() {
                self.playout.restart_stream(packet.timestamp);
            }
            self.source_change_pending = false;
            self.source = Some(packet.ssrc);
            self.playout
                .insert(packet.timestamp, packet.payload, self.codec, Instant::now());
            self.last_received = Instant::now();
        }
        Ok(())
    }

    fn report(&mut self, final_report: bool) {
        let stats = self.playout.stats;
        if stats != self.reported || final_report {
            tracing::info!(peer = %self.peer.address, signaled_peer = %self.signaled_address, codec = ?self.codec, final_report,
                late_samples = stats.late_samples,
                overflow_samples = stats.overflow_samples,
                duplicate_samples = stats.duplicate_samples,
                missing_samples = stats.missing_samples,
                received_packets = self.received_packets,
                malformed_packets = self.malformed_packets,
                unexpected_payload_packets = self.unexpected_payload_packets,
                unexpected_peer_packets = self.unexpected_peer_packets,
                unselected_source_packets = self.unselected_source_packets,
                "G.711 receive quality");
        }
        self.reported = stats;
        self.last_report = Instant::now();
    }
}

impl Drop for AudioNetwork {
    fn drop(&mut self) {
        self.report(true);
    }
}

struct Rtp<'a> {
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &'a [u8],
}

impl<'a> Rtp<'a> {
    fn parse(packet: &'a [u8]) -> Option<Self> {
        if packet.len() < 12 || packet[0] >> 6 != 2 {
            return None;
        }
        let mut start = 12 + (packet[0] as usize & 15) * 4;
        if packet[0] & 0x10 != 0 {
            let extension = packet.get(start..start + 4)?;
            start += 4 + u16::from_be_bytes([extension[2], extension[3]]) as usize * 4;
        }
        let end = match packet[0] & 0x20 != 0 {
            true => {
                let padding = *packet.last()? as usize;
                if padding == 0 {
                    return None;
                }
                packet.len().checked_sub(padding)?
            }
            false => packet.len(),
        };
        Some(Self {
            payload_type: packet[1] & 0x7f,
            sequence: u16::from_be_bytes(packet[2..4].try_into().ok()?),
            timestamp: u32::from_be_bytes(packet[4..8].try_into().ok()?),
            ssrc: u32::from_be_bytes(packet[8..12].try_into().ok()?),
            payload: packet.get(start..end)?,
        })
    }
}

pub(crate) struct PacketNetwork {
    socket: DatagramSocket,
    peer: Peer,
    mapping: Option<Rc<RefCell<Mapping>>>,
    sequence: u16,
    history: VecDeque<Vec<u8>>,
    receiver: RedundancyReceiver,
    max_datagram: usize,
    pub last_activity: Instant,
    pub transmitted_bytes: u64,
}

impl PacketNetwork {
    pub fn new(
        socket: &DatagramSocket,
        address: SocketAddr,
        max_datagram: usize,
        symmetric: bool,
        mapping: Option<Rc<RefCell<Mapping>>>,
    ) -> Result<Self> {
        let socket = socket.try_clone()?;

        tracing::debug!(peer = %address, max_datagram, "UDPTL socket connected");
        Ok(Self {
            socket,
            peer: Peer::new(address, symmetric),
            mapping,
            sequence: 0,
            history: VecDeque::new(),
            receiver: RedundancyReceiver::default(),
            max_datagram,
            last_activity: Instant::now(),
            transmitted_bytes: 0,
        })
    }

    pub fn send(&mut self, packet: IfpPacket) -> Result<()> {
        let mut envelope = UdptlPacket {
            seq_number: self.sequence,
            primary_ifp: packet.payload.clone(),
            error_recovery: ErrorRecovery::Redundancy(
                self.history.iter().rev().take(3).cloned().collect(),
            ),
        };
        let encoded = loop {
            let encoded = envelope.encode();
            if encoded.len() <= self.max_datagram {
                break encoded;
            }
            match &mut envelope.error_recovery {
                ErrorRecovery::Redundancy(history) if !history.is_empty() => {
                    history.pop();
                }
                _ => {
                    return Err(Error::Sip(
                        "T.38 IFP exceeds the peer's maximum datagram size".into(),
                    ));
                }
            }
        };
        for _ in 0..packet.repetitions.clamp(1, 6) {
            self.socket.send_to(&encoded, self.peer.address)?;
            self.transmitted_bytes += encoded.len() as u64;
        }
        // T.38 is half duplex: transmitting a page can legitimately leave RX
        // silent for more than 30 seconds. Only T30_DATA extends the watchdog
        // on TX; periodic indicators (including no-signal) are not progress.
        if packet.payload.first().is_some_and(|byte| byte & 0x40 != 0) {
            self.last_activity = Instant::now();
        }
        tracing::trace!(
            direction = "TX",
            sequence = self.sequence,
            bytes = encoded.len(),
            repetitions = packet.repetitions.clamp(1, 6),
            "UDPTL packet"
        );
        self.sequence = self.sequence.wrapping_add(1);
        self.history.push_back(packet.payload);
        while self.history.len() > 3 {
            self.history.pop_front();
        }
        Ok(())
    }

    pub fn receive(&mut self) -> Result<Vec<udptl::ReceivedIfp>> {
        let mut result = Vec::new();
        let mut buffer = [0_u8; 65536];
        for _ in 0..64 {
            let (size, source) = match self.socket.recv_from(&mut buffer) {
                Ok(value) => value,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            };
            if let Some(mapping) = &self.mapping
                && mapping.borrow_mut().receive(source, &buffer[..size])
            {
                continue;
            }
            match UdptlPacket::decode(&buffer[..size]) {
                Ok(packet) => {
                    if packet.primary_ifp.is_empty()
                        || !self.peer.accept(source, packet.seq_number, 0)
                    {
                        continue;
                    }
                    let recovered = self.receiver.receive(&packet);
                    tracing::trace!(
                        direction = "RX",
                        sequence = packet.seq_number,
                        bytes = size,
                        delivered_ifp = recovered.len(),
                        "UDPTL packet"
                    );
                    // Duplicate datagrams must not keep a stalled call alive.
                    if !recovered.is_empty() {
                        self.last_activity = Instant::now();
                    }
                    result.extend(recovered);
                }
                Err(error) => {
                    tracing::debug!(%error, bytes = size, "Discarding malformed UDPTL packet")
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn same_codec_rtp_restart_requires_a_valid_stream_and_preserves_its_samples() -> Result<()> {
        let local = UdpSocket::bind("127.0.0.1:0")?;
        local.set_nonblocking(true)?;
        let address = local.local_addr()?;
        let socket = DatagramSocket::new(local)?;
        let peer = UdpSocket::bind("127.0.0.1:0")?;
        let other = UdpSocket::bind("127.0.0.1:0")?;
        let mut network =
            AudioNetwork::new(&socket, peer.local_addr()?, G711::Pcmu, false, None, 40)?;
        let inject =
            |network: &mut AudioNetwork, sender: &UdpSocket, ssrc: u32, seq: u16| -> Result<()> {
                let mut packet = vec![0x80, G711::Pcmu.payload_type()];
                packet.extend(seq.to_be_bytes());
                packet.extend((u32::from(seq) * 160).to_be_bytes());
                packet.extend(ssrc.to_be_bytes());
                packet.extend(G711::Pcmu.encode([1000; FRAME_SAMPLES]));
                sender.send_to(&packet, address)?;
                let deadline = Instant::now() + Duration::from_secs(1);
                let mut buffer = [0; 2048];
                while socket.peek_from(&mut buffer).is_err() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                network.receive_packets()
            };
        inject(&mut network, &peer, 111, 5000)?;
        // Gateways can replace their RTP source when switching to fax, even
        // after rejecting T.38 without changing the G.711 codec or SDP.
        inject(&mut network, &peer, 222, 1)?;
        inject(&mut network, &peer, 222, 1)?; // A duplicate is not confirmation.
        inject(&mut network, &other, 222, 2)?;
        assert_eq!(network.source, Some(111));
        network.last_received = Instant::now() - Duration::from_secs(31);
        inject(&mut network, &peer, 222, 2)?;
        assert_eq!(network.source, Some(111));
        inject(&mut network, &peer, 222, 3)?;
        assert_eq!(network.source, Some(222));
        assert!(network.last_received.elapsed() < Duration::from_secs(1));
        let expected = G711::Pcmu.decode(G711::Pcmu.encode([1000; FRAME_SAMPLES]));
        assert_eq!(network.playout.drain(), vec![expected; 4]);
        // Late packets from the replaced stream cannot switch us back.
        for seq in 5001..5004 {
            inject(&mut network, &peer, 111, seq)?;
        }
        assert_eq!(network.source, Some(222));
        assert!(network.playout.drain().is_empty());
        assert_eq!(network.playout.stats.missing_samples, 0);
        assert_eq!(network.playout.stats.overflow_samples, 0);
        Ok(())
    }

    #[test]
    fn codec_renegotiation_accepts_a_new_ssrc_on_the_same_endpoint() -> Result<()> {
        // The reported capture switches PCMA -> PCMU and SSRC at the same
        // address/port. A restarted sender may also choose a new timestamp base.
        for timestamp in [2_055_680_u32, 123] {
            let local = UdpSocket::bind("127.0.0.1:0")?;
            local.set_nonblocking(true)?;
            let address = local.local_addr()?;
            let socket = DatagramSocket::new(local)?;
            let peer = UdpSocket::bind("127.0.0.1:0")?;
            let other_peer = UdpSocket::bind("127.0.0.1:0")?;
            let mut network =
                AudioNetwork::new(&socket, peer.local_addr()?, G711::Pcma, false, None, 40)?;
            let inject = |network: &mut AudioNetwork,
                          sender: &UdpSocket,
                          ssrc: u32,
                          timestamp: u32,
                          codec: G711|
             -> Result<()> {
                let mut packet = vec![0x80, codec.payload_type(), 0, 1];
                packet.extend(timestamp.to_be_bytes());
                packet.extend(ssrc.to_be_bytes());
                packet.extend(codec.encode([1000; FRAME_SAMPLES]));
                sender.send_to(&packet, address)?;
                let deadline = Instant::now() + Duration::from_secs(1);
                let mut buffer = [0; 2048];
                while socket.peek_from(&mut buffer).is_err() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                network.receive_packets()
            };
            let old_ssrc = 0x2fa7_9dfa;
            let new_ssrc = 0x1b83_8de4;
            inject(&mut network, &peer, old_ssrc, 2_054_720, G711::Pcma)?;
            network.update_remote(peer.local_addr()?, G711::Pcmu);
            // Delayed old-codec packets and unrelated endpoints must not select
            // the stream for the newly negotiated codec.
            inject(&mut network, &peer, old_ssrc, 2_054_880, G711::Pcma)?;
            inject(&mut network, &other_peer, 999, 0, G711::Pcmu)?;
            network.last_received = Instant::now() - Duration::from_secs(31);
            inject(&mut network, &peer, new_ssrc, timestamp, G711::Pcmu)?;
            assert_eq!(network.source, Some(new_ssrc));
            assert!(network.last_received.elapsed() < Duration::from_secs(1));
            assert_eq!(
                network.playout.drain(),
                vec![
                    G711::Pcma.decode(G711::Pcma.encode([1000; FRAME_SAMPLES])),
                    G711::Pcmu.decode(G711::Pcmu.encode([1000; FRAME_SAMPLES])),
                ],
                "a new SSRC must retain buffered audio and rebase its timestamp"
            );
            assert_eq!(network.playout.stats.missing_samples, 0);
            assert_eq!(network.playout.stats.overflow_samples, 0);
            // A single unsolicited packet cannot replace the selected stream.
            inject(&mut network, &peer, old_ssrc, timestamp + 160, G711::Pcmu)?;
            assert_eq!(network.source, Some(new_ssrc));
            assert!(network.playout.drain().is_empty());
        }
        Ok(())
    }

    #[test]
    fn audio_endpoint_update_preserves_tx_clock_and_accepts_a_new_rx_stream() -> Result<()> {
        let local = UdpSocket::bind("127.0.0.1:0")?;
        local.set_nonblocking(true)?;
        let address = local.local_addr()?;
        let socket = DatagramSocket::new(local)?;
        let old_peer = UdpSocket::bind("127.0.0.1:0")?;
        let new_peer = UdpSocket::bind("127.0.0.1:0")?;
        old_peer.set_read_timeout(Some(Duration::from_secs(1)))?;
        new_peer.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut network =
            AudioNetwork::new(&socket, old_peer.local_addr()?, G711::Pcma, false, None, 40)?;
        let inject = |network: &mut AudioNetwork,
                      peer: &UdpSocket,
                      ssrc: u32,
                      timestamp: u32,
                      codec: G711|
         -> Result<()> {
            let mut packet = vec![0x80, codec.payload_type(), 0, 1];
            packet.extend(timestamp.to_be_bytes());
            packet.extend(ssrc.to_be_bytes());
            packet.extend(codec.encode([1000; FRAME_SAMPLES]));
            peer.send_to(&packet, address)?;
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut buffer = [0; 2048];
            while socket.peek_from(&mut buffer).is_err() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            network.receive_packets()
        };
        inject(&mut network, &old_peer, 111, 80_000, G711::Pcma)?;
        network.send([0; FRAME_SAMPLES])?;
        let mut old_packet = [0; 172];
        old_peer.recv(&mut old_packet)?;
        network.update_remote(new_peer.local_addr()?, G711::Pcma);
        inject(&mut network, &old_peer, 111, 80_160, G711::Pcma)?;
        assert_eq!(
            network.source, None,
            "the old endpoint must no longer be accepted"
        );
        inject(&mut network, &new_peer, 222, 123, G711::Pcma)?;
        assert_eq!(network.source, Some(222));
        assert_eq!(
            network.playout.drain().len(),
            2,
            "a new clock must preserve buffered audio without a timestamp gap"
        );
        network.send([0; FRAME_SAMPLES])?;
        let mut new_packet = [0; 172];
        new_peer.recv(&mut new_packet)?;
        let before = Rtp::parse(&old_packet).unwrap();
        let after = Rtp::parse(&new_packet).unwrap();
        assert_eq!(after.ssrc, before.ssrc);
        assert_eq!(after.sequence, before.sequence.wrapping_add(1));
        assert_eq!(
            after.timestamp,
            before.timestamp.wrapping_add(FRAME_SAMPLES as u32)
        );
        network.update_remote(new_peer.local_addr()?, G711::Pcmu);
        assert_eq!(
            network.source,
            Some(222),
            "codec changes preserve the receive clock"
        );
        inject(&mut network, &new_peer, 222, 283, G711::Pcmu)?;
        let decoded = network.playout.drain();
        assert_eq!(
            decoded,
            vec![G711::Pcmu.decode(G711::Pcmu.encode([1000; FRAME_SAMPLES]))]
        );
        Ok(())
    }

    #[test]
    fn t38_watchdog_tracks_data_but_not_outgoing_idle_indicators() -> Result<()> {
        let socket = DatagramSocket::new(UdpSocket::bind("127.0.0.1:0")?)?;
        let peer = UdpSocket::bind("127.0.0.1:0")?;
        let mut network = PacketNetwork::new(&socket, peer.local_addr()?, 400, false, None)?;
        let stale = Instant::now() - Duration::from_secs(31);
        network.last_activity = stale;
        network.send(IfpPacket {
            payload: vec![0],
            repetitions: 3,
        })?;
        assert_eq!(network.last_activity, stale, "no-signal is not TX progress");
        assert!(network.last_activity.elapsed() > Duration::from_secs(30));
        // IFP v0: T30_DATA with one HDLC payload octet.
        network.send(IfpPacket {
            payload: vec![0xc8, 1, 0x80, 0, 0, 0xff],
            repetitions: 1,
        })?;
        assert!(
            network.last_activity > stale,
            "active TX must prevent a false RX timeout"
        );
        assert!(network.transmitted_bytes > 0);
        Ok(())
    }

    #[test]
    fn t38_duplicate_packets_do_not_reset_watchdog() -> Result<()> {
        let local = UdpSocket::bind("127.0.0.1:0")?;
        local.set_nonblocking(true)?;
        let address = local.local_addr()?;
        let socket = DatagramSocket::new(local)?;
        let wait_for_packet = || {
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut buffer = [0; 2048];
            while socket.peek_from(&mut buffer).is_err() {
                assert!(Instant::now() < deadline, "loopback packet did not arrive");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let peer = UdpSocket::bind("127.0.0.1:0")?;
        let mut network = PacketNetwork::new(&socket, peer.local_addr()?, 400, false, None)?;
        let bytes = UdptlPacket::with_redundancy(0, vec![0], vec![]).encode();
        peer.send_to(&bytes, address)?;
        wait_for_packet();
        assert_eq!(network.receive()?.len(), 1);
        let stale = Instant::now() - Duration::from_secs(31);
        network.last_activity = stale;
        peer.send_to(&bytes, address)?;
        wait_for_packet();
        assert!(network.receive()?.is_empty());
        assert_eq!(network.last_activity, stale);
        peer.send_to(
            &UdptlPacket::with_redundancy(1, vec![0], vec![]).encode(),
            address,
        )?;
        wait_for_packet();
        assert_eq!(network.receive()?.len(), 1);
        assert!(network.last_activity > stale);
        Ok(())
    }
}
