use faxe_engine::{Cancellation, DocumentInput, DocumentOptions, Documents};
use faxe_native::{AudioFax, FaxEvent, G711, PacketFax};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use udptl::{RedundancyReceiver, UdptlPacket};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Upgrade,
    Reject,
    CancelOffer,
    LateAnswer,
    PeerUpgrade,
    Direct,
}

#[test]
fn sip_registration_auto_t38_and_g711_fallback_deliver_a_page() -> Result<()> {
    run_scenarios(&[
        Scenario::Upgrade,
        Scenario::Reject,
        Scenario::CancelOffer,
        Scenario::LateAnswer,
        Scenario::PeerUpgrade,
    ])
}

#[test]
fn sip_tcp_registration_and_direct_t38_deliver_a_page() -> Result<()> {
    run_scenarios(&[Scenario::Direct])
}

#[test]
fn sip_options_and_peer_bye_reasons_work_over_udp_and_tcp() -> Result<()> {
    for tcp in [false, true] {
        for reason in [
            "Reason: SIP ;cause=408 ;text=\"Request Timeout\"\r\n",
            "Reason: Q.850;cause=16;text=\"Normal, clearing\"\r\nReason: SIP;cause=408;text=\"Request Timeout\"\r\n",
            "",
        ] {
            let mut sip = FixtureSignaling::bind(tcp)?;
            let port = sip.local_addr()?.port();
            let audio = UdpSocket::bind("127.0.0.1:0")?;
            let cancelled = Arc::new(AtomicBool::new(false));
            let stop = cancelled.clone();
            let (result_tx, result_rx) = mpsc::channel();
            let sender = thread::spawn(move || {
                let result = faxe_native::send(
                    faxe_native::SendRequest {
                        account: faxe_native::Account {
                            server: "127.0.0.1",
                            port,
                            transport: match tcp {
                                true => faxe_native::SignalingTransport::Tcp,
                                false => faxe_native::SignalingTransport::Udp,
                            },
                            username: "sender",
                            auth_username: None,
                            password: None,
                            register: true,
                            outbound_proxy: None,
                        },
                        destination: "3003",
                        file: std::path::Path::new("unused-before-media-start.tiff"),
                        station_id: "FAXE TEST",
                        mode: faxe_native::Mode::Auto,
                    },
                    || stop.load(Ordering::Relaxed),
                    |_| {},
                );
                result_tx.send(result).ok();
            });
            let outcome = (|| -> Result<()> {
                let deadline = Instant::now() + Duration::from_secs(180);
                let mut buffer = [0_u8; 65536];
                let mut pending_registration = None;
                let mut registration_probe = String::new();
                let mut dialog = None;
                let mut probe_replies = 0;
                let mut saw_bye_response = false;
                let mut sent_dialog_probe = false;
                loop {
                    if Instant::now() > deadline {
                        return Err("SIP control fixture timed out".into());
                    }
                    while let Ok((size, source)) = sip.recv_from(&mut buffer) {
                        let request = String::from_utf8_lossy(&buffer[..size]);
                        match request.split_whitespace().next().unwrap_or_default() {
                            "REGISTER" if pending_registration.is_none() && probe_replies == 0 => {
                                let contact = header(&request, "Contact")
                                    .unwrap()
                                    .trim_matches(['<', '>']);
                                let transport = match tcp {
                                    true => "TCP",
                                    false => "UDP",
                                };
                                registration_probe = format!(
                                    "OPTIONS {contact} SIP/2.0\r\nVia: SIP/2.0/{transport} 127.0.0.1:{port};branch=z9hG4bKprobe;rport\r\nFrom: <sip:probe@127.0.0.1>;tag=probe\r\nTo: <{contact}>\r\nCall-ID: options-before-registration\r\nCSeq: 1 OPTIONS\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
                                );
                                pending_registration = Some(request.to_string());
                                sip.send_to(registration_probe.as_bytes(), source)?;
                            }
                            "REGISTER" => {
                                sip.send_to(
                                    response(&request, 200, "OK", "", "").as_bytes(),
                                    source,
                                )?;
                            }
                            "INVITE" => {
                                dialog = Some(request.to_string());
                                let body = format!(
                                    "v=0\r\no=fixture 1 1 IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n",
                                    audio.local_addr()?.port()
                                );
                                let extra = format!(
                                    "Contact: <sip:receiver@127.0.0.1:{port}>\r\nContent-Type: application/sdp\r\n"
                                );
                                sip.send_to(
                                    response(&request, 200, "OK", &extra, &body).as_bytes(),
                                    source,
                                )?;
                            }
                            "ACK" if !sent_dialog_probe => {
                                sent_dialog_probe = true;
                                let probe =
                                    peer_request(dialog.as_ref().unwrap(), "OPTIONS", port, "");
                                sip.send_to(probe.as_bytes(), source)?;
                            }
                            "SIP/2.0" => {
                                assert_eq!(
                                    request.split_whitespace().nth(1),
                                    Some("200"),
                                    "{request}"
                                );
                                match header(&request, "CSeq").unwrap() {
                                    "1 OPTIONS" => {
                                        assert_eq!(
                                            header(&request, "Call-ID"),
                                            Some("options-before-registration")
                                        );
                                        probe_replies += 1;
                                        match probe_replies {
                                            1 => {
                                                sip.send_to(registration_probe.as_bytes(), source)?;
                                            }
                                            2 => {
                                                sip.send_to(
                                                    response(
                                                        pending_registration.as_ref().unwrap(),
                                                        200,
                                                        "OK",
                                                        "",
                                                        "",
                                                    )
                                                    .as_bytes(),
                                                    source,
                                                )?;
                                            }
                                            _ => panic!("unexpected duplicate response"),
                                        }
                                    }
                                    "50 OPTIONS" => {
                                        probe_replies += 1;
                                        assert_eq!(
                                            header(&request, "Call-ID"),
                                            header(dialog.as_ref().unwrap(), "Call-ID")
                                        );
                                        let bye =
                                            peer_request(dialog.as_ref().unwrap(), "BYE", port, "")
                                                .replace("CSeq: 50 BYE", "CSeq: 51 BYE")
                                                .replace(
                                                    "Content-Length:",
                                                    &format!("{reason}Content-Length:"),
                                                );
                                        sip.send_to(bye.as_bytes(), source)?;
                                    }
                                    "51 BYE" => saw_bye_response = true,
                                    value => panic!("unexpected response CSeq: {value}"),
                                }
                            }
                            _ => (),
                        }
                    }
                    if let Ok(result) = result_rx.try_recv() {
                        let error = result
                            .expect_err("peer BYE cannot complete a fax")
                            .to_string();
                        assert!(
                            error.contains("Peer ended call before T.30 completion"),
                            "{error}"
                        );
                        match reason.is_empty() {
                            true => assert!(error.contains("without a Reason header"), "{error}"),
                            false => {
                                for line in reason.lines() {
                                    assert!(error.contains(line), "{error}");
                                }
                            }
                        }
                        assert_eq!(probe_replies, 3);
                        assert!(saw_bye_response);
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            })();
            cancelled.store(true, Ordering::Relaxed);
            sender.join().expect("sender panicked");
            outcome?;
        }
    }
    Ok(())
}

fn run_scenarios(scenarios: &[Scenario]) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let root = tempfile::tempdir()?;
    let input = root.path().join("page.png");
    image::GrayImage::from_fn(200, 300, |x, y| {
        image::Luma([match (x, y) {
            (20..=180, 30..=260) if y % 30 < 5 => 0,
            _ => 255,
        }])
    })
    .save(&input)?;
    let documents = Documents::new(root.path().join("spool"))?;
    let prepared = documents.prepare(
        DocumentInput {
            paths: vec![input],
            options: DocumentOptions::default(),
        },
        &Cancellation::default(),
        |_| {},
    )?;
    for &scenario in scenarios {
        eprintln!("SIP fixture: {scenario:?}");
        let mut sip = FixtureSignaling::bind(scenario == Scenario::Direct)?;
        let audio = UdpSocket::bind("127.0.0.1:0")?;
        audio.set_nonblocking(true)?;
        let packets = UdpSocket::bind("127.0.0.1:0")?;
        packets.set_nonblocking(true)?;
        let port = sip.local_addr()?.port();
        let file = documents.fax_path(prepared.id);
        let output = root.path().join(format!("received-{scenario:?}.tiff"));
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = stop.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let sender = thread::spawn(move || {
            let result = faxe_native::send(
                faxe_native::SendRequest {
                    account: faxe_native::Account {
                        server: "127.0.0.1",
                        port,
                        transport: match scenario {
                            Scenario::Direct => faxe_native::SignalingTransport::Tcp,
                            _ => faxe_native::SignalingTransport::Udp,
                        },
                        username: "sender",
                        auth_username: None,
                        password: Some("fixture-password"),
                        register: true,
                        outbound_proxy: None,
                    },
                    destination: "3003",
                    file: &file,
                    station_id: "FAXE TEST",
                    mode: match scenario {
                        Scenario::Direct => faxe_native::Mode::T38,
                        _ => faxe_native::Mode::Auto,
                    },
                },
                || cancel.load(Ordering::Relaxed),
                |_| {},
            );
            result_tx.send(result).ok();
        });
        let outcome = (|| -> Result<()> {
            let mut modem = None;
            let mut terminal = None;
            let mut audio_peer = None;
            let mut packet_peer = None;
            let mut recovery = RedundancyReceiver::default();
            let mut history = VecDeque::new();
            let mut sequence = 0_u16;
            let mut audio_sequence = 0_u16;
            let mut audio_timestamp = 0_u32;
            let mut saw_authorization = false;
            let mut saw_t38_offer = false;
            let mut received_pages = 0;
            let mut pending_offer = None;
            let mut dialog = None;
            let mut peer_offered = false;
            let mut last_tick = Instant::now();
            let deadline = Instant::now() + Duration::from_secs(180);
            let mut buffer = [0_u8; 65536];
            loop {
                if Instant::now() > deadline {
                    return Err("local SIP fax fixture timed out".into());
                }
                while let Ok((size, source)) = sip.recv_from(&mut buffer) {
                    let request = String::from_utf8_lossy(&buffer[..size]);
                    let method = request.split_whitespace().next().unwrap_or_default();
                    match method {
                        "REGISTER" => {
                            let authorized = header(&request, "Authorization").is_some();
                            saw_authorization |= authorized;
                            let response = match authorized {
                                true => response(
                                    &request,
                                    200,
                                    "OK",
                                    &format!(
                                        "Contact: {}\r\nExpires: 300\r\n",
                                        header(&request, "Contact").unwrap()
                                    ),
                                    "",
                                ),
                                false => response(
                                    &request,
                                    401,
                                    "Unauthorized",
                                    "WWW-Authenticate: Digest realm=\"faxe-test\", nonce=\"fixture-nonce\", algorithm=MD5, qop=\"auth\"\r\n",
                                    "",
                                ),
                            };
                            sip.send_to(response.as_bytes(), source)?;
                        }
                        "INVITE" => {
                            let body = request
                                .split_once("\r\n\r\n")
                                .map(|(_, body)| body)
                                .unwrap_or_default();
                            let t38 = body.contains("m=image");
                            saw_t38_offer |= t38;
                            if t38 && scenario == Scenario::Reject {
                                sip.send_to(
                                    response(&request, 488, "Not Acceptable Here", "", "")
                                        .as_bytes(),
                                    source,
                                )?;
                                continue;
                            }
                            if t38
                                && matches!(scenario, Scenario::CancelOffer | Scenario::LateAnswer)
                            {
                                sip.send_to(
                                    response(&request, 100, "Trying", "", "").as_bytes(),
                                    source,
                                )?;
                                pending_offer = Some(request.to_string());
                                continue;
                            }
                            if !t38 {
                                dialog = Some(request.to_string());
                            }
                            let remote_port: u16 = body
                                .lines()
                                .find(|line| {
                                    line.starts_with(match t38 {
                                        true => "m=image ",
                                        false => "m=audio ",
                                    })
                                })
                                .unwrap()
                                .split_whitespace()
                                .nth(1)
                                .unwrap()
                                .parse()?;
                            let media = match t38 {
                                true => {
                                    modem = None;
                                    terminal = Some(PacketFax::receiver(&output, "FIXTURE")?);
                                    packet_peer =
                                        Some(SocketAddr::from(([127, 0, 0, 1], remote_port)));
                                    format!(
                                        "m=image {} udptl t38\r\na=T38FaxVersion:0\r\na=T38MaxBitRate:14400\r\na=T38FaxRateManagement:transferredTCF\r\na=T38FaxMaxDatagram:1200\r\na=T38FaxUdpEC:t38UDPRedundancy\r\n",
                                        packets.local_addr()?.port()
                                    )
                                }
                                false => {
                                    modem = Some(AudioFax::receiver(&output, "FIXTURE")?);
                                    audio_peer =
                                        Some(SocketAddr::from(([127, 0, 0, 1], remote_port)));
                                    format!(
                                        "m=audio {} RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=ptime:20\r\na=sendrecv\r\n",
                                        audio.local_addr()?.port()
                                    )
                                }
                            };
                            let sdp = format!(
                                "v=0\r\no=fixture 1 {} IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n{media}",
                                match t38 {
                                    true => 2,
                                    false => 1,
                                }
                            );
                            let extra = format!(
                                "Contact: <sip:receiver@127.0.0.1:{port}>\r\nContent-Type: application/sdp\r\n"
                            );
                            sip.send_to(
                                response(&request, 200, "OK", &extra, &sdp).as_bytes(),
                                source,
                            )?;
                        }
                        "BYE" => {
                            sip.send_to(response(&request, 200, "OK", "", "").as_bytes(), source)?;
                        }
                        "CANCEL" => {
                            sip.send_to(response(&request, 200, "OK", "", "").as_bytes(), source)?;
                            if let Some(offer) = pending_offer.take() {
                                match scenario {
                                    Scenario::LateAnswer => {
                                        modem = None;
                                        terminal = Some(PacketFax::receiver(&output, "FIXTURE")?);
                                        packet_peer = Some(SocketAddr::from((
                                            [127, 0, 0, 1],
                                            image_port(&offer)?,
                                        )));
                                        let extra = format!(
                                            "Contact: <sip:receiver@127.0.0.1:{port}>\r\nContent-Type: application/sdp\r\n"
                                        );
                                        sip.send_to(
                                            response(
                                                &offer,
                                                200,
                                                "OK",
                                                &extra,
                                                &t38_sdp(packets.local_addr()?.port()),
                                            )
                                            .as_bytes(),
                                            source,
                                        )?;
                                    }
                                    _ => {
                                        sip.send_to(
                                            response(&offer, 487, "Request Terminated", "", "")
                                                .as_bytes(),
                                            source,
                                        )?;
                                    }
                                }
                            }
                        }
                        "ACK" if scenario == Scenario::PeerUpgrade && !peer_offered => {
                            peer_offered = true;
                            let original = dialog.as_ref().unwrap();
                            let body = t38_sdp(packets.local_addr()?.port());
                            sip.send_to(
                                peer_request(original, "INVITE", port, &body).as_bytes(),
                                source,
                            )?;
                        }
                        "SIP/2.0" if scenario == Scenario::PeerUpgrade => {
                            let code: u16 = request.split_whitespace().nth(1).unwrap().parse()?;
                            if code == 200 && header(&request, "CSeq").unwrap().ends_with("INVITE")
                            {
                                modem = None;
                                terminal = Some(PacketFax::receiver(&output, "FIXTURE")?);
                                packet_peer =
                                    Some(SocketAddr::from(([127, 0, 0, 1], image_port(&request)?)));
                                sip.send_to(
                                    peer_request(dialog.as_ref().unwrap(), "ACK", port, "")
                                        .as_bytes(),
                                    source,
                                )?;
                            } else if code >= 300 {
                                return Err(format!("Peer T.38 offer rejected: {code}").into());
                            }
                        }
                        _ => (),
                    }
                }
                if let Some(modem) = &mut modem {
                    while let Ok((size, _)) = audio.recv_from(&mut buffer) {
                        if size == 172 {
                            let mut samples =
                                G711::Pcma.decode(buffer[12..size].try_into().unwrap());
                            modem.receive(&mut samples);
                        }
                    }
                }
                if let Some(terminal) = &mut terminal {
                    while let Ok((size, _)) = packets.recv_from(&mut buffer) {
                        let packet = UdptlPacket::decode(&buffer[..size])?;
                        for packet in recovery.receive(&packet) {
                            terminal.receive(packet.sequence, &packet.payload)?;
                        }
                    }
                }
                if last_tick.elapsed() >= Duration::from_millis(20) {
                    last_tick += Duration::from_millis(20);
                    if let (Some(modem), Some(peer)) = (&mut modem, audio_peer) {
                        let mut packet = vec![0x80, 8];
                        packet.extend(audio_sequence.to_be_bytes());
                        packet.extend(audio_timestamp.to_be_bytes());
                        packet.extend(12345_u32.to_be_bytes());
                        packet.extend(G711::Pcma.encode(modem.transmit()));
                        audio.send_to(&packet, peer)?;
                        audio_sequence = audio_sequence.wrapping_add(1);
                        audio_timestamp = audio_timestamp.wrapping_add(160);
                        for event in modem.events()? {
                            if let FaxEvent::Completed(result) = event {
                                received_pages = result?.received_pages;
                            }
                        }
                    }
                    if let (Some(terminal), Some(peer)) = (&mut terminal, packet_peer) {
                        terminal.tick();
                        for packet in terminal.packets() {
                            let envelope = UdptlPacket::with_redundancy(
                                sequence,
                                packet.payload.clone(),
                                history.iter().rev().cloned().collect(),
                            );
                            packets.send_to(&envelope.encode(), peer)?;
                            history.push_back(packet.payload);
                            while history.len() > 3 {
                                history.pop_front();
                            }
                            sequence = sequence.wrapping_add(1);
                        }
                        for event in terminal.events()? {
                            if let FaxEvent::Completed(result) = event {
                                received_pages = result?.received_pages;
                            }
                        }
                    }
                }
                if let Ok(result) = result_rx.try_recv() {
                    assert_eq!(result?.sent_pages, 1);
                    assert_eq!(received_pages, 1);
                    assert!(
                        saw_authorization,
                        "REGISTER never answered the digest challenge"
                    );
                    assert!(saw_t38_offer || peer_offered, "Auto did not negotiate T.38");
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        })();
        stop.store(true, Ordering::Relaxed);
        sender.join().expect("sender panicked");
        outcome?;
        let mut expected =
            tiff::decoder::Decoder::new(std::fs::File::open(documents.fax_path(prepared.id))?)?;
        let mut received_bytes = std::fs::read(&output)?;
        let mut metadata = tiff::decoder::Decoder::new(std::io::Cursor::new(&received_bytes))?;
        // SpanDSP writes LSB-first strips; the Rust TIFF decoder assumes MSB-first.
        if metadata.get_tag_u32(tiff::tags::Tag::FillOrder)? == 2 {
            let offsets = metadata.get_tag_u64_vec(tiff::tags::Tag::StripOffsets)?;
            let lengths = metadata.get_tag_u64_vec(tiff::tags::Tag::StripByteCounts)?;
            drop(metadata);
            for (offset, length) in offsets.into_iter().zip(lengths) {
                for byte in &mut received_bytes[offset as usize..(offset + length) as usize] {
                    *byte = byte.reverse_bits();
                }
            }
        }
        let mut received = tiff::decoder::Decoder::new(std::io::Cursor::new(received_bytes))?;
        assert_eq!(received.dimensions()?, expected.dimensions()?);
        match (expected.read_image()?, received.read_image()?) {
            (
                tiff::decoder::DecodingResult::U8(expected),
                tiff::decoder::DecodingResult::U8(received),
            ) => assert_eq!(
                received, expected,
                "Received fax pixels differ from the prepared page"
            ),
            _ => panic!("expected bilevel TIFF samples"),
        }
    }
    Ok(())
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

fn image_port(message: &str) -> Result<u16> {
    Ok(message
        .lines()
        .find(|line| line.starts_with("m=image "))
        .ok_or("No image media")?
        .split_whitespace()
        .nth(1)
        .ok_or("No image port")?
        .parse()?)
}

fn t38_sdp(port: u16) -> String {
    format!(
        "v=0\r\no=fixture 1 2 IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=image {port} udptl t38\r\na=T38FaxVersion:0\r\na=T38MaxBitRate:14400\r\na=T38FaxRateManagement:transferredTCF\r\na=T38FaxMaxDatagram:1200\r\na=T38FaxUdpEC:t38UDPRedundancy\r\n"
    )
}

fn peer_request(original: &str, method: &str, port: u16, body: &str) -> String {
    let transport = header(original, "Via")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let target = header(original, "Contact")
        .unwrap()
        .trim_start_matches('<')
        .trim_end_matches('>');
    let from = header(original, "To").unwrap();
    let to = header(original, "From").unwrap();
    let id = header(original, "Call-ID").unwrap();
    format!(
        "{method} {target} SIP/2.0\r\nVia: {transport} 127.0.0.1:{port};branch=z9hG4bK-fixture-{method}\r\nMax-Forwards: 70\r\nFrom: {from};tag=faxe-fixture\r\nTo: {to}\r\nCall-ID: {id}\r\nCSeq: 50 {method}\r\nContact: <sip:receiver@127.0.0.1:{port}>\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn response(request: &str, code: u16, reason: &str, extra: &str, body: &str) -> String {
    let mut response = format!("SIP/2.0 {code} {reason}\r\n");
    for name in ["Via", "From", "To", "Call-ID", "CSeq"] {
        let value = header(request, name).unwrap();
        response.push_str(&format!("{name}: {value}"));
        if name == "To" && !value.contains(";tag=") {
            response.push_str(";tag=faxe-fixture");
        }
        response.push_str("\r\n");
    }
    response.push_str(extra);
    response.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    response
}

enum FixtureSignaling {
    Udp(UdpSocket),
    Tcp {
        listener: TcpListener,
        stream: Option<TcpStream>,
        buffered: Vec<u8>,
    },
}

impl FixtureSignaling {
    fn bind(tcp: bool) -> std::io::Result<Self> {
        match tcp {
            true => {
                let listener = TcpListener::bind("127.0.0.1:0")?;
                listener.set_nonblocking(true)?;
                Ok(Self::Tcp {
                    listener,
                    stream: None,
                    buffered: Vec::new(),
                })
            }
            false => {
                let socket = UdpSocket::bind("127.0.0.1:0")?;
                socket.set_nonblocking(true)?;
                Ok(Self::Udp(socket))
            }
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
            Self::Tcp {
                stream: Some(stream),
                ..
            } => {
                let mut stream = stream;
                stream.write_all(bytes)?;
                Ok(bytes.len())
            }
            Self::Tcp { .. } => Err(std::io::ErrorKind::NotConnected.into()),
        }
    }

    fn recv_from(&mut self, bytes: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        match self {
            Self::Udp(socket) => socket.recv_from(bytes),
            Self::Tcp {
                listener,
                stream,
                buffered,
            } => {
                if stream.is_none() {
                    let (accepted, _) = listener.accept()?;
                    accepted.set_nonblocking(true)?;
                    *stream = Some(accepted);
                }
                let stream = stream.as_mut().unwrap();
                loop {
                    if let Some(end) = buffered.windows(4).position(|chunk| chunk == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buffered[..end]);
                        let body = header(&headers, "Content-Length")
                            .unwrap_or("0")
                            .parse::<usize>()
                            .unwrap();
                        let size = end + 4 + body;
                        if buffered.len() >= size {
                            bytes[..size].copy_from_slice(&buffered[..size]);
                            buffered.drain(..size);
                            return Ok((size, stream.peer_addr()?));
                        }
                    }
                    let size = stream.read(bytes)?;
                    if size == 0 {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                    buffered.extend_from_slice(&bytes[..size]);
                }
            }
        }
    }
}
