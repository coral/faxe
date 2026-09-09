//! Owned receive metadata. No native state or callback handles cross this boundary.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiveCompletion {
    Completed { code: i32 },
    Interrupted { last_status: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceivePageKind {
    Complete,
    RecoveredPartial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivePage {
    pub kind: ReceivePageKind,
    pub decoded_rows: u32,
    pub pixel_width: u32,
    /// Pixels per metre, not DPI.
    pub horizontal_resolution: i32,
    /// Pixels per metre, not DPI.
    pub vertical_resolution: i32,
    pub decoder_bad_rows: u32,
    pub missing_tail: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiveOutputError {
    Write,
    Flush,
    Close,
    Allocation,
    Open,
    Unknown(i32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiveReport {
    pub completion: ReceiveCompletion,
    pub confirmed_complete_pages: u32,
    pub pages: Vec<ReceivePage>,
    pub output_closed: bool,
    pub output_errors: Vec<ReceiveOutputError>,
    pub missing_tail: bool,
    pub unsupported_partial_codec: bool,
}

impl ReceiveReport {
    /// A readable TIFF does not imply a successful T.30 exchange.
    pub fn incomplete_reason(&self) -> Option<String> {
        let mut reasons = Vec::new();
        match self.completion {
            ReceiveCompletion::Completed { code: 0 } => {}
            ReceiveCompletion::Completed { code } => {
                reasons.push(format!("T.30 failed with code {code}"))
            }
            ReceiveCompletion::Interrupted { last_status } => {
                reasons.push(format!("T.30 interrupted (last status {last_status})"))
            }
        }
        if !self.output_closed || !self.output_errors.is_empty() {
            reasons.push(format!(
                "TIFF output did not finish cleanly: {:?}",
                self.output_errors
            ));
        }
        if self.missing_tail
            || self
                .pages
                .iter()
                .any(|p| p.missing_tail || p.kind == ReceivePageKind::RecoveredPartial)
        {
            reasons.push("An unfinished page has missing image content".into());
        }
        if self.pages.iter().any(|p| p.decoder_bad_rows > 0) {
            reasons.push("Received image contains damaged rows".into());
        }
        if self.unsupported_partial_codec {
            reasons.push("The negotiated codec cannot preserve unfinished pages".into());
        }
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }
}

impl From<&spandsp::ReceiveReport> for ReceiveReport {
    fn from(report: &spandsp::ReceiveReport) -> Self {
        use spandsp::ReceiveOutputError as E;
        let mut errors = Vec::new();
        if let Some(error) = report.output_error {
            for (flag, value) in [
                (E::WRITE, ReceiveOutputError::Write),
                (E::FLUSH, ReceiveOutputError::Flush),
                (E::CLOSE, ReceiveOutputError::Close),
                (E::ALLOCATION, ReceiveOutputError::Allocation),
                (E::OPEN, ReceiveOutputError::Open),
            ] {
                if error.contains(flag) {
                    errors.push(value);
                }
            }
            let unknown = error.bits() & !E::all().bits();
            if unknown != 0 {
                errors.push(ReceiveOutputError::Unknown(unknown));
            }
        }
        Self {
            completion: match report.completion {
                spandsp::ReceiveCompletion::Completed { code } => {
                    ReceiveCompletion::Completed { code }
                }
                spandsp::ReceiveCompletion::Interrupted { last_status } => {
                    ReceiveCompletion::Interrupted { last_status }
                }
            },
            confirmed_complete_pages: report.confirmed_complete_pages,
            pages: report
                .pages
                .iter()
                .map(|page| ReceivePage {
                    kind: match page.kind {
                        spandsp::ReceivePageKind::Complete => ReceivePageKind::Complete,
                        spandsp::ReceivePageKind::RecoveredPartial => {
                            ReceivePageKind::RecoveredPartial
                        }
                    },
                    decoded_rows: page.decoded_rows,
                    pixel_width: page.pixel_width,
                    horizontal_resolution: page.horizontal_resolution,
                    vertical_resolution: page.vertical_resolution,
                    decoder_bad_rows: page.decoder_bad_rows,
                    missing_tail: page.missing_tail,
                })
                .collect(),
            output_closed: report.output_closed,
            output_errors: errors,
            missing_tail: report.missing_tail,
            unsupported_partial_codec: report.unsupported_partial_codec,
        }
    }
}
