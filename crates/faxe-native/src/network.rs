use crate::sip::io::DatagramSocket;
use crate::{
    Error, FRAME_SAMPLES, G711, IfpPacket, Result,
    nat::{Mapping, Peer},
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    io::ErrorKind,
    net::{IpAddr, SocketAddr, UdpSocket},
    rc::Rc,
    time::Instant,
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
    mapping: Option<Rc<RefCell<Mapping>>>,
    codec: G711,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    source: Option<u32>,
    anchor: Option<u32>,
    cursor: i64,
    samples: BTreeMap<i64, i16>,
    pub last_received: Instant,
}

impl AudioNetwork {
    pub fn new(
        socket: &DatagramSocket,
        address: SocketAddr,
        codec: G711,
        symmetric: bool,
        mapping: Option<Rc<RefCell<Mapping>>>,
    ) -> Result<Self> {
        let socket = socket.try_clone()?;

        let random = *uuid::Uuid::new_v4().as_bytes();
        tracing::debug!(peer = %address, ?codec, "RTP socket connected");
        Ok(Self {
            socket,
            peer: Peer::new(address, symmetric),
            mapping,
            codec,
            sequence: u16::from_be_bytes([random[0], random[1]]),
            timestamp: u32::from_be_bytes(random[2..6].try_into().unwrap()),
            ssrc: u32::from_be_bytes(random[6..10].try_into().unwrap()),
            source: None,
            anchor: None,
            cursor: -320,
            samples: BTreeMap::new(),
            last_received: Instant::now(),
        })
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

    pub fn receive(&mut self) -> Result<[i16; FRAME_SAMPLES]> {
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
                tracing::trace!(bytes = size, "Discarding malformed RTP packet");
                continue;
            };
            tracing::trace!(
                direction = "RX",
                timestamp = packet.timestamp,
                ssrc = packet.ssrc,
                payload_type = packet.payload_type,
                bytes = size,
                "RTP packet"
            );
            if packet.payload_type != self.codec.payload_type() || packet.payload.len() > 3200 {
                continue;
            }
            if !self.peer.accept(source, packet.sequence, packet.ssrc) {
                continue;
            }
            if self.source.is_some_and(|source| source != packet.ssrc) {
                continue;
            }
            self.source = Some(packet.ssrc);
            let anchor = *self.anchor.get_or_insert(packet.timestamp);
            let offset = i64::from(packet.timestamp.wrapping_sub(anchor) as i32);
            if offset > self.cursor + 8000 {
                continue;
            }
            for (index, byte) in packet.payload.iter().enumerate() {
                let position = offset + index as i64;
                if position >= self.cursor {
                    self.samples
                        .entry(position)
                        .or_insert_with(|| self.codec.decode_sample(*byte));
                }
            }
            self.last_received = Instant::now();
        }
        let frame = std::array::from_fn(|index| {
            self.samples
                .remove(&(self.cursor + index as i64))
                .unwrap_or(0)
        });
        if self.anchor.is_some() {
            self.cursor += FRAME_SAMPLES as i64;
        }
        Ok(frame)
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
    pub last_received: Instant,
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
            last_received: Instant::now(),
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
                    result.extend(recovered);
                    self.last_received = Instant::now();
                }
                Err(error) => {
                    tracing::debug!(%error, bytes = size, "Discarding malformed UDPTL packet")
                }
            }
        }
        Ok(result)
    }
}
