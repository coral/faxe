use faxe_native::{
    AdmissionRequest, ReceiveConfig, ReceiveEvent, ReceiveState, ServiceAccount,
    SignalingTransport, SipService,
};
use std::{
    net::UdpSocket,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
fn header<'a>(message: &'a str, name: &str) -> &'a str {
    message
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .unwrap_or("")
}
#[test]
fn pending_admission_is_busy_times_out_and_rejects_a_late_success() -> TestResult {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.set_nonblocking(true)?;
    let (service, events) = SipService::start()?;
    let (offers, requests) = crossbeam_channel::unbounded::<AdmissionRequest>();
    service.configure(Some(ReceiveConfig {
        account: ServiceAccount {
            id: Uuid::new_v4(),
            server: "127.0.0.1".into(),
            port: socket.local_addr()?.port(),
            transport: SignalingTransport::Udp,
            username: "fixture".into(),
            auth_username: None,
            password: None,
            register: true,
            outbound_proxy: None,
            audio_playout_delay_ms: 200,
            automatic_nat: false,
            stun_server: None,
        },
        station_id: "FAXE".into(),
        mode: faxe_native::Mode::G711,
        ecm: true,
        offer: Arc::new(move |request| {
            offers.send(request).unwrap();
        }),
    }))?;
    let mut contact = String::new();
    let mut target = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut ready = false;
    let mut bytes = [0; 65536];
    while Instant::now() < deadline && !ready {
        while let Ok((size, source)) = socket.recv_from(&mut bytes) {
            let message = String::from_utf8_lossy(&bytes[..size]);
            if message.starts_with("REGISTER") {
                contact = header(&message, "Contact").trim_matches(['<', '>']).into();
                target = Some(source);
                let response = format!(
                    "SIP/2.0 200 OK\r\nVia: {}\r\nFrom: {}\r\nTo: {};tag=fixture\r\nCall-ID: {}\r\nCSeq: {}\r\nContact: {}\r\nExpires: 300\r\nContent-Length: 0\r\n\r\n",
                    header(&message, "Via"),
                    header(&message, "From"),
                    header(&message, "To"),
                    header(&message, "Call-ID"),
                    header(&message, "CSeq"),
                    header(&message, "Contact")
                );
                socket.send_to(response.as_bytes(), source)?;
            }
        }
        ready = events.try_iter().any(|e| {
            matches!(
                e,
                ReceiveEvent::Status {
                    state: ReceiveState::Ready,
                    ..
                }
            )
        });
        thread::sleep(Duration::from_millis(2));
    }
    assert!(ready, "registration never became ready");
    let invite = |id: &str| {
        let body = "v=0\r\no=fixture 1 1 IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 41000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
        format!(
            "INVITE {contact} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:{};branch=z9hG4bK-{id};rport\r\nFrom: <sip:caller@localhost>;tag=caller\r\nTo: <{contact}>\r\nCall-ID: {id}\r\nCSeq: 1 INVITE\r\nContact: <sip:caller@127.0.0.1:{}>\r\nMax-Forwards: 70\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
            socket.local_addr().unwrap().port(),
            socket.local_addr().unwrap().port(),
            body.len()
        )
    };
    socket.send_to(invite("slow").as_bytes(), target.unwrap())?;
    let request = requests.recv_timeout(Duration::from_secs(2))?;
    let id = request.id;
    assert!(request.is_pending());
    socket.send_to(invite("busy").as_bytes(), target.unwrap())?;
    let deadline = Instant::now() + Duration::from_secs(6);
    let (mut provisional, mut busy, mut rejected) = (false, false, false);
    while Instant::now() < deadline && !(busy && rejected) {
        while let Ok((size, _)) = socket.recv_from(&mut bytes) {
            let message = String::from_utf8_lossy(&bytes[..size]);
            match header(&message, "Call-ID") {
                "slow" => {
                    assert!(!message.starts_with("SIP/2.0 200"));
                    provisional |= message.starts_with("SIP/2.0 100");
                    rejected |= message.starts_with("SIP/2.0 480");
                }
                "busy" => busy |= message.starts_with("SIP/2.0 486"),
                _ => (),
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        provisional && busy && rejected,
        "provisional={provisional}, busy={busy}, rejected={rejected}"
    );
    assert!(!request.is_pending());
    let root = tempfile::tempdir()?;
    request.respond(Ok(root.path().join("late.tiff")))?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut closed = false;
    while Instant::now() < deadline && !closed {
        closed=events.try_iter().any(|e|matches!(e,ReceiveEvent::Finished {id:done,outcome:Err(faxe_native::Error::Cancelled),..} if done==id));
        thread::sleep(Duration::from_millis(2));
    }
    assert!(closed, "late persisted admission was not closed");
    service.shutdown();
    Ok(())
}
