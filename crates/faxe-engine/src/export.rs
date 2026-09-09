use crate::{Documents, Error, ExportStatus, ReceivedFax, ReceptionOutcome, Result};
use image::{DynamicImage, GrayImage};
use pdfium_render::prelude::*;
use std::{
    fs::{self, File},
    io::{Cursor, Read, Seek, Write},
    path::Path,
};
use tiff::{
    decoder::{Decoder, DecodingResult, ifd::Value},
    tags::Tag,
};

fn pdf_error(error: PdfiumError) -> Error {
    Error::Pdf(error.to_string())
}
fn resolution<R: Read + Seek>(decoder: &mut Decoder<R>, tag: Tag) -> Result<f32> {
    let value = match decoder.get_tag(tag)? {
        Value::Rational(numerator, denominator) if denominator != 0 => {
            numerator as f32 / denominator as f32
        }
        value => value.into_f64()? as f32,
    };
    let unit = decoder.get_tag_u32(Tag::ResolutionUnit)?;
    let dpi = match unit {
        2 => value,
        3 => value * 2.54,
        _ => {
            return Err(Error::Invalid(
                "TIFF has no physical resolution unit".into(),
            ));
        }
    };
    if !dpi.is_finite() || !(10.0..=2400.0).contains(&dpi) {
        return Err(Error::Invalid("Invalid TIFF resolution".into()));
    }
    Ok(dpi)
}

/// SpanDSP's LSB-first Group 4 strips need normalization for tiff 0.11.
/// Keep the original spool intact; only reverse validated strip byte ranges.
fn normalized_tiff(path: &Path) -> Result<(Vec<u8>, usize, Option<String>)> {
    if fs::metadata(path)?.len() > 128 * 1024 * 1024 {
        return Err(Error::Invalid("Received TIFF exceeds 128 MiB".into()));
    }
    let mut bytes = fs::read(path)?;
    let mut metadata = Decoder::new(Cursor::new(&bytes))?;
    let mut ranges = std::collections::BTreeMap::new();
    let mut pages = 0;
    let mut damage = None;
    for _ in 0..250 {
        let strips = (|| -> Result<Vec<(usize, usize)>> {
            if metadata.get_tag_u32(Tag::FillOrder).unwrap_or(1) != 2 {
                return Ok(Vec::new());
            }
            let offsets = metadata.get_tag_u64_vec(Tag::StripOffsets)?;
            let lengths = metadata.get_tag_u64_vec(Tag::StripByteCounts)?;
            if offsets.len() != lengths.len() {
                return Err(Error::Invalid("Invalid TIFF strip table".into()));
            }
            offsets
                .into_iter()
                .zip(lengths)
                .map(|(offset, length)| {
                    let end = offset
                        .checked_add(length)
                        .ok_or_else(|| Error::Invalid("Invalid TIFF strip range".into()))?;
                    if end > bytes.len() as u64 {
                        return Err(Error::Invalid("Truncated TIFF strip".into()));
                    }
                    Ok((offset as usize, end as usize))
                })
                .collect()
        })();
        match strips {
            Ok(strips) => {
                for (start, end) in strips {
                    ranges.insert(start, end);
                }
            }
            Err(error) => {
                damage = Some(error.to_string());
                break;
            }
        }
        pages += 1;
        if !metadata.more_images() {
            break;
        }
        if let Err(error) = metadata.next_image() {
            damage = Some(error.to_string());
            break;
        }
    }
    drop(metadata);
    let mut previous_end = 0;
    for (start, end) in ranges {
        if start < previous_end {
            return Err(Error::Invalid("Overlapping TIFF strips".into()));
        }
        for byte in &mut bytes[start..end] {
            *byte = byte.reverse_bits();
        }
        previous_end = end;
    }
    Ok((bytes, pages, damage))
}

/// Read pages independently. A corrupt tail must not throw away earlier images.
fn render(documents: &Documents, spool: &Path, fax: &mut ReceivedFax) -> Result<bool> {
    let path = spool.join("fax.tiff");
    if !path.exists() || fs::metadata(&path)?.len() == 0 {
        fax.export = ExportStatus::NoContent;
        return Ok(false);
    }
    let (bytes, readable_directories, mut damage) = normalized_tiff(&path)?;
    let mut decoder = Decoder::new(Cursor::new(bytes))?;
    let mut images = Vec::new();
    let mut pixels_read = 0_u64;
    while images.len() < readable_directories {
        let page = (|| -> Result<_> {
            let (width, height) = decoder.dimensions()?;
            pixels_read += u64::from(width) * u64::from(height);
            if width == 0
                || height == 0
                || u64::from(width) * u64::from(height) > 40_000_000
                || pixels_read > 64_000_000
            {
                return Err(Error::Invalid("Received page exceeds image limits".into()));
            }
            let xdpi = resolution(&mut decoder, Tag::XResolution)?;
            let ydpi = resolution(&mut decoder, Tag::YResolution)?;
            let color = decoder.colortype()?;
            let DecodingResult::U8(bytes) = decoder.read_image()? else {
                return Err(Error::Invalid("Unsupported received TIFF pixels".into()));
            };
            // The TIFF decoder normalizes WhiteIsZero, but bilevel pixels remain packed.
            let pixels = match color {
                tiff::ColorType::Gray(1) => {
                    let stride = width.div_ceil(8) as usize;
                    if bytes.len() != stride * height as usize {
                        return Err(Error::Invalid("Incomplete TIFF scanlines".into()));
                    }
                    (0..height as usize)
                        .flat_map(|y| {
                            let bytes = &bytes;
                            (0..width as usize).map(move |x| {
                                if bytes[y * stride + x / 8] & (0x80 >> (x % 8)) != 0 {
                                    255
                                } else {
                                    0
                                }
                            })
                        })
                        .collect()
                }
                tiff::ColorType::Gray(8) => bytes,
                _ => return Err(Error::Invalid("Received TIFF is not monochrome".into())),
            };
            let image = GrayImage::from_raw(width, height, pixels)
                .ok_or_else(|| Error::Invalid("Invalid TIFF dimensions".into()))?;
            Ok((
                DynamicImage::ImageLuma8(image),
                width as f32 * 72.0 / xdpi,
                height as f32 * 72.0 / ydpi,
            ))
        })();
        match page {
            Ok(image) => images.push(image),
            Err(error) => {
                damage = Some(error.to_string());
                break;
            }
        }
        if !decoder.more_images() {
            break;
        }
        if images.len() >= 250 {
            damage = Some("Reception exceeds the 250-page export limit".into());
            break;
        }
        if let Err(error) = decoder.next_image() {
            damage = Some(error.to_string());
            break;
        }
    }
    if images.is_empty() {
        return Err(Error::Invalid(
            damage.unwrap_or_else(|| "No readable image content".into()),
        ));
    }
    fax.recovered_pages = images.len() as u32;
    if let Some(report) = &fax.recovery {
        if let Some(reason) = report.incomplete_reason() {
            damage.get_or_insert(reason);
        }
        if report.pages.len() != images.len() {
            damage.get_or_insert_with(|| {
                "Readable images differ from finalized TIFF inventory".into()
            });
        }
    }
    if fax.outcome.is_complete() && fax.recovered_pages != fax.confirmed_pages {
        damage.get_or_insert_with(|| "Recovered image count differs from confirmed pages".into());
    }
    if let Some(reason) = damage {
        fax.outcome = ReceptionOutcome::Partial {
            reason: format!("Image recovery: {reason}"),
        };
    }
    if !fax.outcome.is_complete() && !matches!(fax.outcome, ReceptionOutcome::Partial { .. }) {
        fax.outcome = ReceptionOutcome::Partial {
            reason: match &fax.outcome {
                ReceptionOutcome::Failed { reason } => reason.clone(),
                ReceptionOutcome::Interrupted => "Reception interrupted".into(),
                _ => "Reception incomplete".into(),
            },
        };
    }
    let pdfium = documents.pdfium()?;
    let mut document = pdfium.create_new_pdf().map_err(pdf_error)?;
    let font = document.fonts_mut().helvetica();
    let partial = !fax.outcome.is_complete();
    for (image, width, height) in images {
        let margin = if partial { 30.0 } else { 0.0 };
        let mut page = document
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::new_custom(
                PdfPoints::new(width.max(300.0)),
                PdfPoints::new(height + margin),
            ))
            .map_err(pdf_error)?;
        page.objects_mut()
            .create_image_object(
                PdfPoints::ZERO,
                PdfPoints::ZERO,
                &image,
                Some(PdfPoints::new(width)),
                Some(PdfPoints::new(height)),
            )
            .map_err(pdf_error)?;
        if partial {
            page.objects_mut()
                .create_text_object(
                    PdfPoints::new(12.0),
                    PdfPoints::new(height + 10.0),
                    "INCOMPLETE FAX - recovered content",
                    font,
                    PdfPoints::new(11.0),
                )
                .map_err(pdf_error)?;
        }
    }
    let mut output = tempfile::NamedTempFile::new_in(spool)?;
    output.write_all(&document.save_to_bytes().map_err(pdf_error)?)?;
    // An opaque per-reception marker makes recovery comparison unambiguous.
    writeln!(output, "\n% FAXE reception {}", fax.id)?;
    output.as_file().sync_all()?;
    let mut report = tempfile::NamedTempFile::new_in(spool)?;
    report.write_all(&serde_json::to_vec(&(fax.recovered_pages, &fax.outcome))?)?;
    report.as_file().sync_all()?;
    report
        .persist(spool.join("render.json"))
        .map_err(|error| error.error)?;
    output
        .persist(spool.join("received.pdf"))
        .map_err(|error| error.error)?;
    sync_directory(spool)?;
    Ok(true)
}
fn same_file_content(a: &Path, b: &Path) -> Result<bool> {
    if !b.exists() || fs::metadata(a)?.len() != fs::metadata(b)?.len() {
        return Ok(false);
    }
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    let (mut left, mut right) = ([0_u8; 16384], [0_u8; 16384]);
    loop {
        let n = a.read(&mut left)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut right[..n])?;
        if left[..n] != right[..n] {
            return Ok(false);
        }
    }
}
fn sync_directory(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(_path)?.sync_all()?;
    Ok(())
}

pub(crate) fn export(
    documents: &Documents,
    spool: &Path,
    fax: &mut ReceivedFax,
    mut journal: impl FnMut(&ReceivedFax) -> Result<()>,
) -> Result<()> {
    if fax.outcome.is_active() {
        return Err(Error::Invalid(
            "Wait for reception to finish before exporting".into(),
        ));
    }
    if matches!(
        fax.export,
        ExportStatus::Published { .. } | ExportStatus::NoContent
    ) {
        return Ok(());
    }
    let internal_pdf = spool.join("received.pdf");
    if internal_pdf.is_file() && spool.join("render.json").is_file() {
        let (pages, outcome): (u32, ReceptionOutcome) =
            serde_json::from_slice(&fs::read(spool.join("render.json"))?)?;
        fax.recovered_pages = pages;
        fax.outcome = outcome;
    }
    if let ExportStatus::Publishing {
        destination,
        staging,
        ..
    } = &fax.export
        && internal_pdf.is_file()
        && same_file_content(&internal_pdf, destination)?
    {
        sync_directory(destination.parent().unwrap())?;
        let staging = staging.clone();
        fax.export = ExportStatus::Published {
            path: destination.clone(),
        };
        journal(fax)?;
        let _ = fs::remove_file(staging);
        return Ok(());
    }
    if !internal_pdf.is_file() && !render(documents, spool, fax)? {
        journal(fax)?;
        return Ok(());
    }
    let folder = fax
        .options
        .folder
        .clone()
        .ok_or_else(|| Error::Invalid("No receive folder configured".into()))?;
    if !folder.is_dir() {
        return Err(Error::Invalid(format!(
            "Receive folder unavailable: {}",
            folder.display()
        )));
    }
    let staging = folder.join(format!(".faxe-{}.pdf", fax.id));
    if !staging.exists() {
        let mut temp = tempfile::NamedTempFile::new_in(&folder)?;
        std::io::copy(&mut File::open(&internal_pdf)?, &mut temp)?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(&staging).map_err(|e| e.error)?;
        sync_directory(&folder)?;
    } else if !same_file_content(&internal_pdf, &staging)? {
        return Err(Error::Invalid(
            "Export staging file changed; internal spool retained".into(),
        ));
    }
    for suffix in 1..=100_000 {
        let name = if suffix == 1 {
            format!("{}.pdf", fax.filename_stem)
        } else {
            format!("{}-{suffix}.pdf", fax.filename_stem)
        };
        let destination = folder.join(name);
        fax.export = ExportStatus::Publishing {
            destination: destination.clone(),
            staging: staging.clone(),
            bytes: fs::metadata(&staging)?.len(),
            error: None,
        };
        journal(fax)?;
        match fs::hard_link(&staging, &destination) {
            Ok(()) => {
                sync_directory(&folder)?;
                fax.export = ExportStatus::Published { path: destination };
                journal(fax)?;
                let _ = fs::remove_file(&staging);
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Invalid(
        "Too many receive filename collisions".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cancellation, DocumentInput, DocumentOptions, ReceiveSettings, Uuid};
    fn record(folder: &Path) -> ReceivedFax {
        ReceivedFax {
            id: Uuid::new_v4(),
            profile_id: Uuid::new_v4(),
            profile_name: "Fixture".into(),
            caller: "Local fixture".into(),
            arrived_at: chrono::Utc::now(),
            filename_stem: "2026-09-07_12-00-00".into(),
            finished_at: Some(chrono::Utc::now()),
            options: ReceiveSettings {
                folder: Some(folder.to_owned()),
                ..Default::default()
            },
            confirmed_pages: 1,
            recovered_pages: 0,
            transport: Some(crate::FaxMode::T38),
            result: Some(crate::ReceptionResult::Failed {
                reason: "Final exchange failed".into(),
                t30_code: None,
            }),
            outcome: ReceptionOutcome::Failed {
                reason: "Final exchange failed".into(),
            },
            export: ExportStatus::Pending,
            partial_page_preservation_available: false,
            recovery: None,
        }
    }
    fn pixel_pages(path: &Path) -> Result<Vec<(u32, u32, Vec<u8>)>> {
        let (bytes, count, damage) = normalized_tiff(path)?;
        assert!(damage.is_none());
        let mut decoder = Decoder::new(Cursor::new(bytes))?;
        let mut pages = Vec::new();
        for i in 0..count {
            let (width, height) = decoder.dimensions()?;
            let color = decoder.colortype()?;
            let DecodingResult::U8(bytes) = decoder.read_image()? else {
                panic!("bilevel pixels")
            };
            let pixels = match color {
                tiff::ColorType::Gray(1) => (0..height as usize)
                    .flat_map(|y| {
                        let bytes = &bytes;
                        (0..width as usize).map(move |x| {
                            if bytes[y * width.div_ceil(8) as usize + x / 8] & (0x80 >> (x % 8))
                                != 0
                            {
                                255
                            } else {
                                0
                            }
                        })
                    })
                    .collect(),
                tiff::ColorType::Gray(8) => bytes,
                _ => panic!("bilevel pixels"),
            };
            pages.push((width, height, pixels));
            if i + 1 < count {
                decoder.next_image()?;
            }
        }
        Ok(pages)
    }

    #[derive(Debug, Clone, Copy)]
    enum Interruption {
        Before,
        First,
        Between,
        Second,
        Success,
    }

    #[test]
    fn finalized_audio_and_t38_recover_exact_rows_and_export_partial_pdfs()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use faxe_native::{
            AudioFax, FaxEvent, G711, PacketFax, ReceiveCompletion, ReceivePageKind,
        };
        let root = tempfile::tempdir()?;
        let documents = Documents::new(root.path().join("documents"))?;
        let source = root.path().join("source.png");
        GrayImage::from_fn(1728, 2292, |x, y| {
            image::Luma([if (x / 8 + y / 9).is_multiple_of(2) {
                0
            } else {
                255
            }])
        })
        .save(&source)?;
        let prepared = documents.prepare(
            DocumentInput {
                paths: vec![source.clone(), source],
                options: DocumentOptions::default(),
            },
            &Cancellation::default(),
            |_| {},
        )?;
        let input = documents.fax_path(prepared.id);
        let expected = pixel_pages(&input)?;
        let folder = root.path().join("pdfs");
        fs::create_dir(&folder)?;
        for ecm in [false, true] {
            for codec in [Some(G711::Pcma), Some(G711::Pcmu), None] {
                for stop in [
                    Interruption::Before,
                    Interruption::First,
                    Interruption::Between,
                    Interruption::Second,
                    Interruption::Success,
                ] {
                    let spool = root.path().join(format!("{codec:?}-{ecm}-{stop:?}"));
                    fs::create_dir(&spool)?;
                    let output = spool.join("fax.tiff");
                    let mut finished = false;
                    let should_stop =
                        |stats: &faxe_native::TransferStats, completed: bool| match stop {
                            Interruption::Before => true,
                            Interruption::First => {
                                stats.received_pages == 0 && stats.received_rows > 200
                            }
                            Interruption::Between => stats.received_pages == 1,
                            Interruption::Second => {
                                stats.received_pages == 1
                                    && stats.received_rows > 200
                                    && stats.received_rows < expected[1].1
                            }
                            Interruption::Success => completed,
                        };
                    let report = if let Some(codec) = codec {
                        let mut tx = AudioFax::transmitter(&input, "SOURCE")?;
                        let mut rx = AudioFax::receiver_with_ecm(&output, "RECEIVER", ecm)?;
                        for _ in 0..30000 {
                            let completed = rx
                                .events()?
                                .iter()
                                .any(|e| matches!(e, FaxEvent::Completed(Ok(_))));
                            if should_stop(&rx.statistics()?, completed) {
                                finished = true;
                                break;
                            }
                            let mut a = codec.decode(codec.encode(tx.transmit()));
                            let mut b = codec.decode(codec.encode(rx.transmit()));
                            rx.receive(&mut a);
                            tx.receive(&mut b);
                        }
                        let report = rx.finalize_receive();
                        let bytes = fs::read(&output).ok();
                        assert_eq!(rx.finalize_receive(), report);
                        assert_eq!(bytes, fs::read(&output).ok());
                        drop(rx);
                        assert_eq!(bytes, fs::read(&output).ok());
                        report
                    } else {
                        let mut tx = PacketFax::transmitter(&input, "SOURCE")?;
                        let mut rx = PacketFax::receiver_with_ecm(&output, "RECEIVER", ecm)?;
                        let (mut a, mut b) = (0u16, 0u16);
                        for _ in 0..30000 {
                            let completed = rx
                                .events()?
                                .iter()
                                .any(|e| matches!(e, FaxEvent::Completed(Ok(_))));
                            if should_stop(&rx.statistics()?, completed) {
                                finished = true;
                                break;
                            }
                            tx.tick();
                            rx.tick();
                            for packet in tx.packets() {
                                rx.receive(a, &packet.payload)?;
                                a = a.wrapping_add(1);
                            }
                            for packet in rx.packets() {
                                tx.receive(b, &packet.payload)?;
                                b = b.wrapping_add(1);
                            }
                        }
                        let report = rx.finalize_receive();
                        let bytes = fs::read(&output).ok();
                        assert_eq!(rx.finalize_receive(), report);
                        assert_eq!(bytes, fs::read(&output).ok());
                        drop(rx);
                        assert_eq!(bytes, fs::read(&output).ok());
                        report
                    };
                    assert!(finished, "{codec:?} ECM={ecm} {stop:?}");
                    assert!(report.output_closed);
                    assert!(report.output_errors.is_empty());
                    let (confirmed, images) = match stop {
                        Interruption::Before => (0, 0),
                        Interruption::First => (0, 1),
                        Interruption::Between => (1, 1),
                        Interruption::Second => (1, 2),
                        Interruption::Success => (2, 2),
                    };
                    assert_eq!(
                        report.confirmed_complete_pages, confirmed,
                        "{codec:?} ECM={ecm} {stop:?}"
                    );
                    assert_eq!(
                        report.pages.len(),
                        images,
                        "{codec:?} ECM={ecm} {stop:?}: {report:?}"
                    );
                    if matches!(stop, Interruption::Success) {
                        assert_eq!(report.completion, ReceiveCompletion::Completed { code: 0 });
                    } else {
                        assert!(matches!(
                            report.completion,
                            ReceiveCompletion::Interrupted { .. }
                        ));
                    }
                    if images > 0 {
                        let recovered = pixel_pages(&output)?;
                        assert_eq!(recovered.len(), images);
                        for (i, ((width, height, pixels), meta)) in
                            recovered.iter().zip(&report.pages).enumerate()
                        {
                            assert_eq!(*width, expected[i].0);
                            assert_eq!(*height, meta.decoded_rows);
                            assert_eq!(*width, meta.pixel_width);
                            assert_eq!(
                                pixels,
                                &expected[i].2[..pixels.len()],
                                "{codec:?} ECM={ecm} {stop:?} page {i}"
                            );
                            assert_eq!(meta.decoder_bad_rows, 0);
                        }
                        if matches!(stop, Interruption::First | Interruption::Second) {
                            let page = report.pages.last().unwrap();
                            assert_eq!(page.kind, ReceivePageKind::RecoveredPartial);
                            assert!(page.missing_tail && page.decoded_rows < expected[0].1);
                        }
                    }
                    let mut fax = record(&folder);
                    fax.confirmed_pages = confirmed;
                    fax.partial_page_preservation_available = true;
                    fax.result = Some(if matches!(stop, Interruption::Success) {
                        crate::ReceptionResult::Succeeded
                    } else {
                        crate::ReceptionResult::Interrupted
                    });
                    fax.outcome = if matches!(stop, Interruption::Success) {
                        ReceptionOutcome::Received
                    } else {
                        ReceptionOutcome::Interrupted
                    };
                    fax.recovery = Some(report.clone());
                    export(&documents, &spool, &mut fax, |_| Ok(()))?;
                    assert_eq!(fax.confirmed_pages, confirmed);
                    assert_eq!(fax.recovered_pages as usize, images);
                    if images == 0 {
                        assert_eq!(fax.export, ExportStatus::NoContent);
                        continue;
                    }
                    let ExportStatus::Published { path } = &fax.export else {
                        panic!("PDF publication")
                    };
                    let pdf = documents
                        .pdfium()?
                        .load_pdf_from_file(path, None)
                        .map_err(pdf_error)?;
                    assert_eq!(pdf.pages().len() as usize, images);
                    for (i, meta) in report.pages.iter().enumerate() {
                        let page = pdf.pages().get(i as i32).map_err(pdf_error)?;
                        let partial = !matches!(stop, Interruption::Success);
                        assert_eq!(
                            page.text()
                                .map_err(pdf_error)?
                                .all()
                                .contains("INCOMPLETE FAX"),
                            partial
                        );
                        let height = meta.decoded_rows as f32 * 72.0
                            / (meta.vertical_resolution as f32 * 0.0254)
                            + if partial { 30.0 } else { 0.0 };
                        assert!((page.height().value - height).abs() < 2.0);
                    }
                    assert_eq!(
                        fax.outcome.is_complete(),
                        matches!(stop, Interruption::Success)
                    );
                    assert_eq!(fax.recovery, Some(report));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn recovered_pages_publish_without_overwrite_and_retry_is_idempotent() -> Result<()> {
        let root = tempfile::tempdir()?;
        let documents = Documents::new(root.path().join("documents"))?;
        let source = root.path().join("source.png");
        GrayImage::from_fn(100, 150, |x, y| {
            image::Luma([if x < 50 && y < 75 { 0 } else { 255 }])
        })
        .save(&source)?;
        let prepared = documents.prepare(
            DocumentInput {
                paths: vec![source],
                options: DocumentOptions::default(),
            },
            &Cancellation::default(),
            |_| {},
        )?;
        let spool = root.path().join("reception");
        fs::create_dir(&spool)?;
        fs::copy(documents.fax_path(prepared.id), spool.join("fax.tiff"))?;
        let folder = root.path().join("published");
        fs::create_dir(&folder)?;
        let occupied = folder.join("2026-09-07_12-00-00.pdf");
        fs::write(&occupied, b"existing file")?;
        let mut fax = record(&folder);
        let mut before_publish = None;
        export(&documents, &spool, &mut fax, |fax| {
            if matches!(fax.export, ExportStatus::Publishing { .. }) {
                before_publish = Some(fax.clone());
            }
            Ok(())
        })?;
        assert!(matches!(fax.outcome, ReceptionOutcome::Partial { .. }));
        assert_eq!(fax.confirmed_pages, 1);
        assert_eq!(fax.recovered_pages, 1);
        let ExportStatus::Published { path } = &fax.export else {
            panic!("PDF not published")
        };
        assert_eq!(path.file_name().unwrap(), "2026-09-07_12-00-00-2.pdf");
        assert_eq!(fs::read(&occupied)?, b"existing file");
        let pdf = documents
            .pdfium()?
            .load_pdf_from_file(path, None)
            .map_err(pdf_error)?;
        let page = pdf.pages().get(0).map_err(pdf_error)?;
        assert!(
            page.text()
                .map_err(pdf_error)?
                .all()
                .contains("INCOMPLETE FAX")
        );
        assert!((page.width().value - 1728.0 * 72.0 / 204.0).abs() < 0.1);
        assert!((page.height().value - (2292.0 * 72.0 / 196.0 + 30.0)).abs() < 2.0);
        // Simulate process death after publication but before updating the database.
        let mut interrupted = before_publish.unwrap();
        export(&documents, &spool, &mut interrupted, |_| Ok(()))?;
        assert_eq!(interrupted.export, fax.export);
        assert_eq!(fs::read_dir(&folder)?.count(), 2);
        Ok(())
    }
    #[test]
    fn zero_content_has_no_pdf_and_unavailable_folder_keeps_internal_output() -> Result<()> {
        let root = tempfile::tempdir()?;
        let documents = Documents::new(root.path().join("documents"))?;
        let mut fax = record(&root.path().join("absent"));
        export(&documents, root.path(), &mut fax, |_| Ok(()))?;
        assert_eq!(fax.export, ExportStatus::NoContent);
        assert!(!root.path().join("received.pdf").exists());
        fax.export = ExportStatus::Pending;
        fs::write(
            root.path().join("received.pdf"),
            b"already converted internal content",
        )?;
        assert!(export(&documents, root.path(), &mut fax, |_| Ok(())).is_err());
        assert!(root.path().join("received.pdf").is_file());
        Ok(())
    }
    #[test]
    fn corrupt_trailing_directory_preserves_the_first_page_and_call_result() -> Result<()> {
        let root = tempfile::tempdir()?;
        let documents = Documents::new(root.path().join("documents"))?;
        let source = root.path().join("source.png");
        GrayImage::from_pixel(100, 100, image::Luma([255])).save(&source)?;
        let prepared = documents.prepare(
            DocumentInput {
                paths: vec![source.clone(), source],
                options: DocumentOptions::default(),
            },
            &Cancellation::default(),
            |_| {},
        )?;
        let mut bytes = fs::read(documents.fax_path(prepared.id))?;
        assert_eq!(&bytes[..2], b"II");
        let first = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let entries = u16::from_le_bytes(bytes[first..first + 2].try_into().unwrap()) as usize;
        let next = first + 2 + entries * 12;
        let second = u32::from_le_bytes(bytes[next..next + 4].try_into().unwrap()) as usize;
        assert!(second > first);
        bytes.truncate(second);
        let spool = root.path().join("recovery");
        fs::create_dir(&spool)?;
        fs::write(spool.join("fax.tiff"), bytes)?;
        let folder = root.path().join("pdfs");
        fs::create_dir(&folder)?;
        let mut fax = record(&folder);
        let result = fax.result.clone();
        fax.confirmed_pages = 2;
        export(&documents, &spool, &mut fax, |_| Ok(()))?;
        assert_eq!(fax.recovered_pages, 1);
        assert_eq!(fax.confirmed_pages, 2);
        assert_eq!(fax.result, result);
        assert!(matches!(fax.outcome, ReceptionOutcome::Partial { .. }));
        assert!(matches!(fax.export, ExportStatus::Published { .. }));
        Ok(())
    }
}
