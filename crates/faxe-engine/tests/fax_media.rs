use faxe_engine::{Cancellation, DocumentInput, DocumentOptions, Documents};
use faxe_native::{AudioFax, FaxEvent, G711, PacketFax, TransferStats};
use image::{GrayImage, Luma};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn completed(events: Vec<FaxEvent>) -> Result<Option<TransferStats>> {
    for event in events {
        if let FaxEvent::Completed(result) = event {
            return Ok(Some(result?));
        }
    }
    Ok(None)
}

#[test]
fn prepared_pages_complete_t30_over_audio_and_t38() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("page.png");
    GrayImage::from_fn(400, 600, |x, y| {
        Luma([match (x, y) {
            (40..=360, 40..=80) => 0,
            (40..=320, 120..=540) if y % 40 < 8 => 0,
            _ => 255,
        }])
    })
    .save(&source)?;
    let documents = Documents::new(directory.path().join("spool"))?;
    let prepared = documents.prepare(
        DocumentInput {
            paths: vec![source.clone(), source],
            options: DocumentOptions::default(),
        },
        &Cancellation::default(),
        |_| {},
    )?;
    let tiff = documents.fax_path(prepared.id);

    for ecm in [true, false] {
        for codec in [G711::Pcma, G711::Pcmu] {
            let audio_output = directory
                .path()
                .join(format!("{codec:?}-{ecm}-received.tiff"));
            let mut sender = AudioFax::transmitter(&tiff, "FAXE SENDER")?;
            let mut receiver = AudioFax::receiver_with_ecm(&audio_output, "FAXE RECEIVER", ecm)?;
            let (mut sent, mut received) = (None, None);
            for _ in 0..15000 {
                let mut outbound = codec.decode(codec.encode(sender.transmit()));
                let mut inbound = codec.decode(codec.encode(receiver.transmit()));
                receiver.receive(&mut outbound);
                sender.receive(&mut inbound);
                sent = sent.or(completed(sender.events()?)?);
                received = received.or(completed(receiver.events()?)?);
                if sent.is_some() && received.is_some() {
                    break;
                }
            }
            assert_eq!(received.as_ref().expect("receiver stats").ecm, ecm);
            assert_eq!(
                sent.expect("audio sender did not complete T.30").sent_pages,
                2
            );
            assert_eq!(
                received
                    .expect("audio receiver did not complete T.30")
                    .received_pages,
                2
            );
            drop((sender, receiver));
            assert!(std::fs::metadata(audio_output)?.len() > 0);
        }

        let packet_output = directory.path().join(format!("t38-{ecm}-received.tiff"));
        let mut sender = PacketFax::transmitter(&tiff, "FAXE SENDER")?;
        let mut receiver = PacketFax::receiver_with_ecm(&packet_output, "FAXE RECEIVER", ecm)?;
        let (mut sent, mut received) = (None, None);
        let (mut tx_sequence, mut rx_sequence) = (0_u16, 0_u16);
        for _ in 0..15000 {
            sender.tick();
            receiver.tick();
            for packet in sender.packets() {
                receiver.receive(tx_sequence, &packet.payload)?;
                tx_sequence = tx_sequence.wrapping_add(1);
            }
            for packet in receiver.packets() {
                sender.receive(rx_sequence, &packet.payload)?;
                rx_sequence = rx_sequence.wrapping_add(1);
            }
            sent = sent.or(completed(sender.events()?)?);
            received = received.or(completed(receiver.events()?)?);
            if sent.is_some() && received.is_some() {
                break;
            }
        }
        assert_eq!(received.as_ref().expect("receiver stats").ecm, ecm);
        assert_eq!(
            sent.expect("T.38 sender did not complete T.30").sent_pages,
            2
        );
        assert_eq!(
            received
                .expect("T.38 receiver did not complete T.30")
                .received_pages,
            2
        );
        drop((sender, receiver));
        assert!(std::fs::metadata(packet_output)?.len() > 0);
    }
    Ok(())
}
