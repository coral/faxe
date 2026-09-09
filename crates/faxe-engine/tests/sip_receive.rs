use faxe_engine::*;
use faxe_native::{AudioFax, FaxEvent, G711, PacketFax};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use udptl::{RedundancyReceiver, UdptlPacket};
type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
struct NoCredentials;
impl Credentials for NoCredentials {
    fn password(&self, _: Uuid) -> Result<Option<Password>> {
        Ok(None)
    }
}
fn header<'a>(message: &'a str, name: &str) -> &'a str {
    message
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .unwrap_or("")
}
fn response(request: &str, code: &str, extra: &str, body: &str) -> String {
    format!(
        "SIP/2.0 {code}\r\nVia: {}\r\nFrom: {}\r\nTo: {};tag=registrar\r\nCall-ID: {}\r\nCSeq: {}\r\n{extra}Content-Length: {}\r\n\r\n{body}",
        header(request, "Via"),
        header(request, "From"),
        header(request, "To"),
        header(request, "Call-ID"),
        header(request, "CSeq"),
        body.len()
    )
}
fn request(
    method: &str,
    contact: &str,
    port: u16,
    id: &str,
    to: &str,
    body: &str,
    cseq: u32,
) -> String {
    format!(
        "{method} {contact} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:{port};branch=z9hG4bK-{id}-{cseq}-{method};rport\r\nFrom: <sip:caller@127.0.0.1>;tag=caller\r\nTo: {to}\r\nCall-ID: {id}\r\nCSeq: {cseq} {method}\r\nMax-Forwards: 70\r\nContact: <sip:caller@127.0.0.1:{port}>\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}
#[test]
fn idle_receiver_renews_answers_immediately_receives_t38_and_keeps_outgoing_isolated() -> TestResult
{
    run_receive(FaxMode::Auto, None, true, false)
}
#[test]
fn receives_pcma_and_pcmu_and_auto_upgrade_or_rejection() -> TestResult {
    run_receive(FaxMode::G711, Some(G711::Pcma), false, false)?;
    run_receive(FaxMode::G711, Some(G711::Pcmu), false, false)?;
    run_receive(FaxMode::Auto, Some(G711::Pcma), false, false)?;
    run_receive(FaxMode::Auto, Some(G711::Pcma), true, false)
}
#[test]
fn receives_on_registered_tcp_flow() -> TestResult {
    run_receive(FaxMode::T38, None, true, true)
}
static FIXTURE: Mutex<()> = Mutex::new(());
fn run_receive(mode: FaxMode, codec: Option<G711>, accept_upgrade: bool, tcp: bool) -> TestResult {
    let _fixture = FIXTURE.lock().unwrap_or_else(|error| error.into_inner());
    eprintln!("Receive scenario: {mode:?}, {codec:?}, accept_upgrade={accept_upgrade}");
    let root = tempfile::tempdir()?;
    let folder = root.path().join("received");
    std::fs::create_dir(&folder)?;
    let mut sip = Signaling::bind(tcp)?;
    let packet_socket = UdpSocket::bind("127.0.0.1:0")?;
    packet_socket.set_nonblocking(true)?;
    let port = sip.local_addr()?.port();
    let concurrent = codec.is_none() && !tcp;
    let outbound_socket = UdpSocket::bind("127.0.0.1:0")?;
    outbound_socket.set_nonblocking(true)?;
    let mut outbound_peer = None;
    let mut outbound_receiver =
        PacketFax::receiver(&root.path().join("outgoing-received.tiff"), "OUTGOING PEER")?;
    let mut outbound_sequence = 0_u16;
    let mut outbound_recovery = RedundancyReceiver::default();
    let mut outbound_completed = false;
    let engine = Engine::open(
        root.path().join("engine"),
        Arc::new(NoCredentials),
        Some(Arc::new(SipSender::new()?)),
    )?;
    let profile = SipProfile {
        id: Uuid::new_v4(),
        name: "Receive fixture".into(),
        server: "127.0.0.1".into(),
        port,
        transport: if tcp {
            SipTransport::Tcp
        } else {
            SipTransport::Udp
        },
        username: "inbox".into(),
        auth_username: None,
        register: true,
        outbound_proxy: None,
        station_id: "INBOX".into(),
        automatic_nat: true,
        stun_server: None,
        sending_mode: Default::default(),
    };
    engine.save_profile(&profile)?;
    let mut other = profile.clone();
    other.id = Uuid::new_v4();
    other.username = "outbound".into();
    other.name = "Other account".into();
    engine.save_profile(&other)?;
    let source = root.path().join("page.png");
    image::GrayImage::from_fn(100, 100, |x, y| {
        image::Luma([if x % 20 < 4 && y % 20 < 4 { 0 } else { 255 }])
    })
    .save(&source)?;
    let documents = engine.documents();
    let prepared = documents.prepare(
        DocumentInput {
            paths: vec![source],
            options: DocumentOptions::default(),
        },
        &Cancellation::default(),
        |_| {},
    )?;
    let source_tiff = documents.fax_path(prepared.id);
    let mut transmitter = PacketFax::transmitter(&source_tiff, "SENDER")?;
    let mut audio_transmitter = AudioFax::transmitter(&source_tiff, "SENDER")?;
    let mut audio_sequence = 0_u16;
    let mut audio_timestamp = 0_u32;
    let mut audio_peer = None;
    let mut audio_samples = std::collections::VecDeque::new();
    let mut upgraded = codec.is_none();
    let initial_sdp = codec.map(|codec| format!("v=0\r\no=fixture 1 1 IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP {}\r\na=rtpmap:{} {}/8000\r\n", packet_socket.local_addr().unwrap().port(), codec.payload_type(), codec.payload_type(), if codec == G711::Pcma { "PCMA" } else { "PCMU" })).unwrap_or_else(|| t38(packet_socket.local_addr().unwrap().port()));
    engine.configure_receiver(ReceiveSettings {
        profile: Some(profile.id),
        folder: Some(folder.clone()),
        mode,
        ..Default::default()
    })?;
    let deadline = Instant::now() + Duration::from_secs(100);
    let mut buffer = [0_u8; 65536];
    let mut contact = String::new();
    let mut peer: Option<SocketAddr> = None;
    let mut remote_media: Option<SocketAddr> = None;
    let mut sent_invite: Option<Instant> = None;
    let mut accepted = false;
    let mut busy = false;
    let mut duplicate = false;
    let mut renewals = 0;
    let mut outgoing_job = None;
    let mut sequence = 0_u16;
    let mut recovery = RedundancyReceiver::default();
    let mut next_frame = Instant::now();
    let mut completed = false;
    let mut dialog_to = String::new();
    while Instant::now() < deadline {
        while let Ok((size, source)) = sip.recv_from(&mut buffer) {
            let message = String::from_utf8_lossy(&buffer[..size]);
            if message.starts_with("REGISTER") {
                let incoming = header(&message, "From").contains("inbox@");
                if incoming {
                    renewals += 1;
                    contact = header(&message, "Contact")
                        .trim_matches(['<', '>'])
                        .to_owned();
                    peer = Some(source);
                }
                let extra = format!(
                    "Contact: {};expires=6\r\nExpires: 6\r\n",
                    header(&message, "Contact")
                );
                sip.send_to(response(&message, "200 OK", &extra, "").as_bytes(), source)?;
            } else if message.starts_with("INVITE") {
                if header(&message, "From").contains("outbound@") {
                    if concurrent {
                        outbound_peer = Some(SocketAddr::from((
                            [127, 0, 0, 1],
                            message
                                .lines()
                                .find(|line| line.starts_with("m=image "))
                                .unwrap()
                                .split_whitespace()
                                .nth(1)
                                .unwrap()
                                .parse::<u16>()?,
                        )));
                        sip.send_to(response(&message, "200 OK", &format!("Contact: <sip:caller@127.0.0.1:{port}>\r\nContent-Type: application/sdp\r\n"), &t38(outbound_socket.local_addr()?.port())).as_bytes(), source)?;
                    } else {
                        sip.send_to(
                            response(&message, "486 Busy Here", "", "").as_bytes(),
                            source,
                        )?;
                    }
                } else if accept_upgrade {
                    remote_media = Some(SocketAddr::from((
                        [127, 0, 0, 1],
                        message
                            .lines()
                            .find(|line| line.starts_with("m=image "))
                            .unwrap()
                            .split_whitespace()
                            .nth(1)
                            .unwrap()
                            .parse::<u16>()?,
                    )));
                    let response = format!(
                        "SIP/2.0 200 OK\r\nVia: {}\r\nFrom: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nContact: <sip:caller@127.0.0.1:{port}>\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
                        header(&message, "Via"),
                        header(&message, "From"),
                        header(&message, "To"),
                        header(&message, "Call-ID"),
                        header(&message, "CSeq"),
                        t38(packet_socket.local_addr()?.port()).len(),
                        t38(packet_socket.local_addr()?.port())
                    );
                    sip.send_to(response.as_bytes(), source)?;
                    upgraded = true;
                } else {
                    let response = format!(
                        "SIP/2.0 488 Not Acceptable Here\r\nVia: {}\r\nFrom: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nContent-Length: 0\r\n\r\n",
                        header(&message, "Via"),
                        header(&message, "From"),
                        header(&message, "To"),
                        header(&message, "Call-ID"),
                        header(&message, "CSeq")
                    );
                    sip.send_to(response.as_bytes(), source)?;
                }
            } else if message.starts_with("SIP/2.0 200")
                && header(&message, "Call-ID") == "incoming-main"
                && header(&message, "CSeq") == "1 INVITE"
            {
                assert!(
                    sent_invite.unwrap().elapsed() < Duration::from_secs(2),
                    "receiver delayed its answer"
                );
                dialog_to = header(&message, "To").to_owned();
                if accepted {
                    duplicate = true;
                } else {
                    accepted = true;
                    let remote = Some(SocketAddr::from((
                        [127, 0, 0, 1],
                        message
                            .lines()
                            .find(|line| {
                                line.starts_with(if codec.is_some() {
                                    "m=audio "
                                } else {
                                    "m=image "
                                })
                            })
                            .unwrap()
                            .split_whitespace()
                            .nth(1)
                            .unwrap()
                            .parse::<u16>()?,
                    )));
                    if codec.is_some() {
                        audio_peer = remote;
                    } else {
                        remote_media = remote;
                    }
                    // A retransmitted initial INVITE must get the same dialog response.
                    sip.send_to(
                        request(
                            "INVITE",
                            &contact,
                            port,
                            "incoming-main",
                            &format!("<{contact}>"),
                            &initial_sdp,
                            1,
                        )
                        .as_bytes(),
                        source,
                    )?;
                    sip.send_to(
                        request(
                            "INVITE",
                            &contact,
                            port,
                            "incoming-busy",
                            &format!("<{contact}>"),
                            &initial_sdp,
                            1,
                        )
                        .as_bytes(),
                        source,
                    )?;
                    outgoing_job = Some(
                        engine
                            .enqueue(FaxRequest {
                                profile_id: other.id,
                                destination: "3003".into(),
                                mode: FaxMode::T38,
                                document: prepared.clone(),
                            })?
                            .id,
                    );
                }
                sip.send_to(
                    request("ACK", &contact, port, "incoming-main", &dialog_to, "", 1).as_bytes(),
                    source,
                )?;
            } else if message.starts_with("SIP/2.0 486")
                && header(&message, "Call-ID") == "incoming-busy"
            {
                busy = true;
            } else if message.starts_with("BYE") {
                sip.send_to(response(&message, "200 OK", "", "").as_bytes(), source)?;
            }
        }
        if !contact.is_empty()
            && sent_invite.is_none()
            && renewals >= 2
            && matches!(
                engine.snapshot()?.receiver_status.state,
                ReceiverState::Ready
            )
        {
            let message = request(
                "INVITE",
                &contact,
                port,
                "incoming-main",
                &format!("<{contact}>"),
                &initial_sdp,
                1,
            );
            sip.send_to(message.as_bytes(), peer.unwrap())?;
            sent_invite = Some(Instant::now());
        }
        while let Ok((size, _)) = packet_socket.recv_from(&mut buffer) {
            if size >= 12 && buffer[0] >> 6 == 2 {
                if let Some(codec) = codec {
                    audio_samples.extend(codec.decode(buffer[12..size].try_into().unwrap()));
                }
            } else if upgraded {
                for packet in recovery.receive(&UdptlPacket::decode(&buffer[..size])?) {
                    transmitter.receive(packet.sequence, &packet.payload)?;
                }
            }
        }
        while let Ok((size, _)) = outbound_socket.recv_from(&mut buffer) {
            for packet in outbound_recovery.receive(&UdptlPacket::decode(&buffer[..size])?) {
                outbound_receiver.receive(packet.sequence, &packet.payload)?;
            }
        }
        if accepted && Instant::now() >= next_frame {
            if next_frame.elapsed() > Duration::from_millis(250) {
                next_frame = Instant::now();
            }
            next_frame += Duration::from_millis(20);
            if let Some(peer) = outbound_peer {
                outbound_receiver.tick();
                for packet in outbound_receiver.packets() {
                    let envelope =
                        UdptlPacket::with_redundancy(outbound_sequence, packet.payload, Vec::new());
                    for _ in 0..packet.repetitions {
                        outbound_socket.send_to(&envelope.encode(), peer)?;
                    }
                    outbound_sequence = outbound_sequence.wrapping_add(1);
                }
                for event in outbound_receiver.events()? {
                    if let FaxEvent::Completed(stats) = event {
                        assert_eq!(stats?.received_pages, 1);
                        outbound_completed = true;
                    }
                }
            }
            if upgraded {
                transmitter.tick();
                if let Some(remote) = remote_media {
                    for packet in transmitter.packets() {
                        let envelope =
                            UdptlPacket::with_redundancy(sequence, packet.payload, Vec::new());
                        for _ in 0..packet.repetitions {
                            packet_socket.send_to(&envelope.encode(), remote)?;
                        }
                        sequence = sequence.wrapping_add(1);
                    }
                }
                for event in transmitter.events()? {
                    if let FaxEvent::Completed(stats) = event {
                        assert_eq!(stats?.sent_pages, 1);
                        completed = true;
                    }
                }
            } else if let (Some(codec), Some(peer)) = (codec, audio_peer) {
                let mut frame = std::array::from_fn(|_| audio_samples.pop_front().unwrap_or(0));
                audio_transmitter.receive(&mut frame);
                let mut packet = vec![0x80, codec.payload_type()];
                packet.extend(audio_sequence.to_be_bytes());
                packet.extend(audio_timestamp.to_be_bytes());
                packet.extend(42_u32.to_be_bytes());
                packet.extend(codec.encode(audio_transmitter.transmit()));
                packet_socket.send_to(&packet, peer)?;
                audio_sequence = audio_sequence.wrapping_add(1);
                audio_timestamp = audio_timestamp.wrapping_add(160);
                for event in audio_transmitter.events()? {
                    if let FaxEvent::Completed(stats) = event {
                        assert_eq!(stats?.sent_pages, 1);
                        completed = true;
                    }
                }
            }
        }
        let snapshot = engine.snapshot()?;
        if let Some(fax) = snapshot.inbox.first()
            && matches!(fax.export, ExportStatus::Published { .. })
            && completed
            && (!concurrent || outbound_completed)
            && snapshot
                .jobs
                .iter()
                .any(|job| Some(job.id) == outgoing_job && job.state.is_finished())
        {
            assert!(
                matches!(fax.outcome, ReceptionOutcome::Received),
                "{:?}",
                fax.outcome
            );
            assert_eq!(fax.confirmed_pages, 1);
            assert_eq!(fax.recovered_pages, 1);
            assert!(fax.partial_page_preservation_available);
            let report = fax
                .recovery
                .as_ref()
                .expect("persisted finalization report");
            assert_eq!(
                report.completion,
                faxe_engine::ReceiveCompletion::Completed { code: 0 }
            );
            assert_eq!(report.confirmed_complete_pages, 1);
            assert_eq!(report.pages.len(), 1);
            assert!(report.output_closed && report.output_errors.is_empty());
            assert_eq!(snapshot.inbox.len(), 1);
            assert!(
                busy && duplicate && renewals >= 2,
                "busy={busy} duplicate={duplicate} renewals={renewals} completed={completed}"
            );
            assert!(snapshot.jobs.iter().any(|job| Some(job.id) == outgoing_job
                && if concurrent {
                    matches!(
                        job.state,
                        JobState::Succeeded {
                            acknowledged_pages: 1
                        }
                    )
                } else {
                    matches!(job.state, JobState::Failed { .. })
                }));
            let ExportStatus::Published { path } = &fax.export else {
                unreachable!()
            };
            assert!(path.is_file());
            engine.clear_queue()?;
            assert_eq!(engine.snapshot()?.inbox.len(), 1);
            engine.configure_receiver(ReceiveSettings {
                profile: None,
                ..snapshot.receive_settings
            })?;
            assert_eq!(engine.snapshot()?.inbox.len(), 1);
            return Ok(());
        }
        if let Some(fax) = snapshot.inbox.first()
            && matches!(
                fax.export,
                ExportStatus::Failed { .. } | ExportStatus::NoContent
            )
        {
            panic!("reception/export failed: {fax:?}");
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!(
        "receive fixture timed out: {:?}, renewals {renewals}, accepted {accepted}, dialog {dialog_to}",
        engine.snapshot()?
    );
}
fn t38(port: u16) -> String {
    format!(
        "v=0\r\no=fixture 1 1 IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=image {port} udptl t38\r\na=T38FaxVersion:0\r\na=T38MaxBitRate:14400\r\na=T38FaxRateManagement:transferredTCF\r\na=T38FaxMaxDatagram:1200\r\na=T38FaxUdpEC:t38UDPRedundancy\r\n"
    )
}

enum Signaling {
    Udp(UdpSocket),
    Tcp {
        listener: TcpListener,
        streams: Vec<(TcpStream, Vec<u8>)>,
    },
}
impl Signaling {
    fn bind(tcp: bool) -> std::io::Result<Self> {
        if tcp {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            listener.set_nonblocking(true)?;
            Ok(Self::Tcp {
                listener,
                streams: Vec::new(),
            })
        } else {
            let socket = UdpSocket::bind("127.0.0.1:0")?;
            socket.set_nonblocking(true)?;
            Ok(Self::Udp(socket))
        }
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Udp(socket) => socket.local_addr(),
            Self::Tcp { listener, .. } => listener.local_addr(),
        }
    }
    fn send_to(&self, bytes: &[u8], address: SocketAddr) -> std::io::Result<usize> {
        match self {
            Self::Udp(socket) => socket.send_to(bytes, address),
            Self::Tcp { streams, listener } => {
                let port = listener.local_addr()?.port();
                let text = String::from_utf8_lossy(bytes)
                    .replace("SIP/2.0/UDP", "SIP/2.0/TCP")
                    .replace(
                        &format!("Contact: <sip:caller@127.0.0.1:{port}>"),
                        &format!("Contact: <sip:caller@127.0.0.1:{port};transport=tcp>"),
                    );
                let mut stream = &streams
                    .iter()
                    .find(|(stream, _)| stream.peer_addr().ok() == Some(address))
                    .ok_or(std::io::ErrorKind::NotConnected)?
                    .0;
                stream.write_all(text.as_bytes())?;
                Ok(bytes.len())
            }
        }
    }
    fn recv_from(&mut self, bytes: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        match self {
            Self::Udp(socket) => socket.recv_from(bytes),
            Self::Tcp { listener, streams } => {
                while let Ok((stream, _)) = listener.accept() {
                    stream.set_nonblocking(true)?;
                    streams.push((stream, Vec::new()));
                }
                for (stream, buffered) in streams {
                    while let Ok(size) = stream.read(bytes) {
                        if size == 0 {
                            break;
                        }
                        buffered.extend_from_slice(&bytes[..size]);
                    }
                    while buffered.starts_with(b"\r\n") {
                        buffered.drain(..2);
                    }
                    if let Some(end) = buffered.windows(4).position(|chunk| chunk == b"\r\n\r\n") {
                        let text = String::from_utf8_lossy(&buffered[..end]);
                        let size = end
                            + 4
                            + header(&text, "Content-Length")
                                .parse::<usize>()
                                .unwrap_or(0);
                        if buffered.len() >= size {
                            bytes[..size].copy_from_slice(&buffered[..size]);
                            buffered.drain(..size);
                            return Ok((size, stream.peer_addr()?));
                        }
                    }
                }
                Err(std::io::ErrorKind::WouldBlock.into())
            }
        }
    }
}
