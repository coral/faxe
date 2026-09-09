use faxe_engine::*;
use std::{
    net::{SocketAddr, UdpSocket},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
struct CredentialsFixture;
impl Credentials for CredentialsFixture {
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
struct Peer {
    socket: UdpSocket,
    contacts: Vec<(String, SocketAddr)>,
    replies: Vec<String>,
    registrations: usize,
}
impl Peer {
    fn new() -> TestResult<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            contacts: Vec::new(),
            replies: Vec::new(),
            registrations: 0,
        })
    }
    fn pump(&mut self) -> TestResult {
        let mut bytes = [0; 65536];
        while let Ok((size, source)) = self.socket.recv_from(&mut bytes) {
            let message = String::from_utf8_lossy(&bytes[..size]).into_owned();
            if message.starts_with("REGISTER") || message.starts_with("BYE") {
                let answer = format!(
                    "SIP/2.0 200 OK\r\nVia: {}\r\nFrom: {}\r\nTo: {};tag=fixture\r\nCall-ID: {}\r\nCSeq: {}\r\nContact: {}\r\nExpires: 300\r\nContent-Length: 0\r\n\r\n",
                    header(&message, "Via"),
                    header(&message, "From"),
                    header(&message, "To"),
                    header(&message, "Call-ID"),
                    header(&message, "CSeq"),
                    header(&message, "Contact")
                );
                self.socket.send_to(answer.as_bytes(), source)?;
                if message.starts_with("REGISTER") && header(&message, "Expires") != "0" {
                    self.registrations += 1;
                    self.contacts.push((
                        header(&message, "Contact").trim_matches(['<', '>']).into(),
                        source,
                    ));
                }
            } else if message.starts_with("SIP/2.0") {
                self.replies.push(message);
            }
        }
        Ok(())
    }
    fn wait(&mut self, mut condition: impl FnMut(&Self) -> bool) -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            self.pump()?;
            if condition(self) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
        Err(format!("control fixture timed out; replies {:?}", self.replies).into())
    }
    fn send(
        &self,
        method: &str,
        id: &str,
        target: &(String, SocketAddr),
        to: &str,
        body: &str,
    ) -> TestResult {
        let port = self.socket.local_addr()?.port();
        let cseq = if method == "BYE" { 2 } else { 1 };
        let reason = if method == "BYE" {
            "Reason: SIP;cause=408;text=\"Control fixture interrupted\"\r\n"
        } else {
            ""
        };
        let request = format!(
            "{method} {} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:{port};branch=z9hG4bK-{id}-{method};rport\r\nFrom: <sip:caller@localhost>;tag=caller\r\nTo: {to}\r\nCall-ID: {id}\r\nCSeq: {cseq} {method}\r\nContact: <sip:caller@127.0.0.1:{port}>\r\nMax-Forwards: 70\r\n{reason}Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
            target.0,
            body.len()
        );
        self.socket.send_to(request.as_bytes(), target.1)?;
        Ok(())
    }
    fn answer(&self, id: &str) -> Option<String> {
        self.replies
            .iter()
            .find(|reply| {
                reply.starts_with("SIP/2.0 200")
                    && header(reply, "Call-ID") == id
                    && header(reply, "CSeq") == "1 INVITE"
            })
            .cloned()
    }
}
#[test]
fn profile_change_is_deferred_cancel_is_isolated_and_shutdown_preserves_interruption() -> TestResult
{
    let root = tempfile::tempdir()?;
    let data = root.path().join("data");
    let first_folder = root.path().join("first");
    let second_folder = root.path().join("second");
    std::fs::create_dir(&first_folder)?;
    std::fs::create_dir(&second_folder)?;
    let mut peer = Peer::new()?;
    let engine = Engine::open(
        data.clone(),
        Arc::new(CredentialsFixture),
        Some(Arc::new(SipSender::new()?)),
    )?;
    let first = SipProfile {
        id: Uuid::new_v4(),
        name: "First".into(),
        server: "127.0.0.1".into(),
        port: peer.socket.local_addr()?.port(),
        transport: SipTransport::Udp,
        username: "first".into(),
        auth_username: None,
        register: true,
        outbound_proxy: None,
        station_id: "FIXTURE".into(),
        automatic_nat: true,
        stun_server: None,
        sending_mode: Default::default(),
    };
    let mut second = first.clone();
    second.id = Uuid::new_v4();
    second.name = "Second".into();
    second.username = "second".into();
    engine.save_profile(&first)?;
    engine.save_profile(&second)?;
    assert!(
        engine
            .configure_receiver(ReceiveSettings {
                profile: Some(first.id),
                folder: Some(root.path().join("unavailable")),
                ..Default::default()
            })
            .is_err()
    );
    engine.configure_receiver(ReceiveSettings {
        profile: Some(first.id),
        folder: Some(first_folder.clone()),
        mode: FaxMode::G711,
        ecm: false,
        ..Default::default()
    })?;
    peer.wait(|_| {
        matches!(
            engine.snapshot().unwrap().receiver_status.state,
            ReceiverState::Ready
        )
    })
    .map_err(|error| {
        format!(
            "{error}; registrations {}; status {:?}",
            peer.registrations,
            engine.snapshot().unwrap().receiver_status
        )
    })?;
    let target = peer.contacts.last().unwrap().clone();
    let audio = "v=0\r\no=fixture 1 1 IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 41000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
    peer.send(
        "INVITE",
        "first-call",
        &target,
        &format!("<{}>", target.0),
        audio,
    )?;
    peer.wait(|peer| peer.answer("first-call").is_some())?;
    let answer = peer.answer("first-call").unwrap();
    peer.send("ACK", "first-call", &target, header(&answer, "To"), "")?;
    peer.wait(|_| {
        matches!(
            engine.snapshot().unwrap().receiver_status.state,
            ReceiverState::Receiving
        )
    })?;
    let fax = engine.snapshot()?.inbox[0].clone();
    engine.configure_receiver(ReceiveSettings {
        profile: Some(second.id),
        folder: Some(second_folder.clone()),
        mode: FaxMode::T38,
        ..Default::default()
    })?;
    peer.send(
        "INVITE",
        "busy-call",
        &target,
        &format!("<{}>", target.0),
        audio,
    )?;
    peer.wait(|peer| {
        peer.replies
            .iter()
            .any(|reply| reply.starts_with("SIP/2.0 486"))
    })?;
    assert_eq!(peer.registrations, 1);
    assert_eq!(engine.snapshot()?.receiver_status.profile, Some(first.id));
    assert_eq!(
        engine.snapshot()?.inbox[0].options.folder,
        Some(first_folder)
    );
    assert!(!fax.options.ecm);
    engine.cancel_reception(fax.id)?;
    peer.wait(|_| {
        let snapshot = engine.snapshot().unwrap();
        snapshot.receiver_status.profile == Some(second.id)
            && matches!(snapshot.receiver_status.state, ReceiverState::Ready)
    })?;
    assert_eq!(engine.snapshot()?.inbox.len(), 1);
    assert!(matches!(
        engine.snapshot()?.inbox[0].result,
        Some(ReceptionResult::Failed { .. })
    ));
    let target = peer
        .contacts
        .iter()
        .rev()
        .find(|(contact, _)| contact.contains("second@"))
        .unwrap()
        .clone();
    // Late offer: answer supplies SDP; ACK carries the peer's SDP answer.
    peer.send(
        "INVITE",
        "late-offer",
        &target,
        &format!("<{}>", target.0),
        "",
    )?;
    peer.wait(|peer| peer.answer("late-offer").is_some())?;
    let answer = peer.answer("late-offer").unwrap();
    assert!(answer.contains("m=image "));
    let t38 = "v=0\r\no=fixture 2 1 IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=image 42000 udptl t38\r\na=T38FaxVersion:0\r\na=T38FaxRateManagement:transferredTCF\r\na=T38FaxUdpEC:t38UDPRedundancy\r\n";
    peer.send("ACK", "late-offer", &target, header(&answer, "To"), t38)?;
    peer.wait(|_| {
        matches!(
            engine.snapshot().unwrap().receiver_status.state,
            ReceiverState::Receiving
        )
    })?;
    peer.send("BYE", "late-offer", &target, header(&answer, "To"), "")?;
    peer.wait(|_| engine.snapshot().unwrap().inbox.iter().any(|fax| matches!(&fax.result, Some(ReceptionResult::Failed { reason, .. }) if reason.contains("Control fixture interrupted"))))?;
    let previous = peer.registrations;
    engine.reconnect_receiver()?;
    peer.wait(|peer| {
        peer.registrations > previous
            && matches!(
                engine.snapshot().unwrap().receiver_status.state,
                ReceiverState::Ready
            )
    })?;
    let target = peer.contacts.last().unwrap().clone();
    peer.send(
        "INVITE",
        "shutdown-call",
        &target,
        &format!("<{}>", target.0),
        t38,
    )?;
    peer.wait(|peer| peer.answer("shutdown-call").is_some())?;
    let answer = peer.answer("shutdown-call").unwrap();
    peer.send("ACK", "shutdown-call", &target, header(&answer, "To"), "")?;
    peer.wait(|_| {
        matches!(
            engine.snapshot().unwrap().receiver_status.state,
            ReceiverState::Receiving
        )
    })?;
    drop(engine);
    let inspection = Engine::open(data, Arc::new(CredentialsFixture), None)?;
    assert!(
        inspection
            .snapshot()?
            .inbox
            .iter()
            .any(|fax| fax.result == Some(ReceptionResult::Interrupted))
    );
    assert!(inspection.snapshot()?.jobs.is_empty());
    let registrations = peer.registrations;
    thread::sleep(Duration::from_millis(50));
    peer.pump()?;
    // Any remaining datagrams are the old service's unregistration, not new work.
    assert_eq!(registrations, peer.registrations);
    Ok(())
}
