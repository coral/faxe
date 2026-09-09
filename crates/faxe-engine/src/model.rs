use std::{fmt, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaxMode {
    #[default]
    Auto,
    T38,
    G711,
}

impl FaxMode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::T38, Self::G711];
}

impl fmt::Display for FaxMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "Auto · prefer T.38",
            Self::T38 => "T.38 only",
            Self::G711 => "G.711 only",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SipTransport {
    #[default]
    Udp,
    Tcp,
    Tls,
}

impl SipTransport {
    pub const ALL: [Self; 3] = [Self::Udp, Self::Tcp, Self::Tls];

    pub fn default_port(self) -> u16 {
        match self {
            Self::Udp | Self::Tcp => 5060,
            Self::Tls => 5061,
        }
    }
}

impl fmt::Display for SipTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Udp => "UDP",
            Self::Tcp => "TCP",
            Self::Tls => "TLS",
        })
    }
}

#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct SipProfile {
    pub id: Uuid,
    pub name: String,
    pub server: String,
    pub port: u16,
    pub transport: SipTransport,
    pub username: String,
    pub auth_username: Option<String>,
    pub register: bool,
    pub outbound_proxy: Option<String>,
    pub station_id: String,
    #[serde(default)]
    pub sending_mode: FaxMode,
    #[serde(default = "enabled")]
    pub automatic_nat: bool,
    #[serde(default)]
    pub stun_server: Option<String>,
    #[serde(default = "default_audio_playout_delay_ms")]
    pub audio_playout_delay_ms: u16,
}

fn default_audio_playout_delay_ms() -> u16 {
    faxe_native::DEFAULT_AUDIO_PLAYOUT_DELAY_MS
}

fn enabled() -> bool {
    true
}

impl SipProfile {
    pub fn validate(&self) -> Result<()> {
        faxe_native::validate_audio_playout_delay(self.audio_playout_delay_ms)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        nonempty("Profile name", &self.name)?;
        nonempty("SIP server", &self.server)?;
        nonempty("SIP username", &self.username)?;
        if self.port == 0 {
            return Err(Error::Invalid(
                "SIP port must be between 1 and 65535".into(),
            ));
        }
        if self
            .server
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '@' | ';' | '?' | '<' | '>'))
        {
            return Err(Error::Invalid(
                "Enter a SIP hostname or IP address, without sip: or a port".into(),
            ));
        }
        if self.server.contains(':') && self.server.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(Error::Invalid(
                "Set the SIP port separately from the server".into(),
            ));
        }
        if self
            .username
            .chars()
            .any(|c| c.is_control() || matches!(c, '@' | '<' | '>' | ';' | '?' | ' '))
        {
            return Err(Error::Invalid(
                "SIP username contains a URI delimiter".into(),
            ));
        }
        if self.station_id.len() > 20
            || !self.station_id.is_ascii()
            || self.station_id.chars().any(char::is_control)
        {
            return Err(Error::Invalid(
                "Station ID must be at most 20 printable ASCII characters".into(),
            ));
        }
        if let Some(proxy) = &self.outbound_proxy {
            validate_sip_uri(proxy)?;
        }
        if self.stun_server.as_ref().is_some_and(|server| {
            server.trim().is_empty()
                || server.chars().any(char::is_whitespace)
                || server.contains('/')
        }) {
            return Err(Error::Invalid(
                "Use a STUN hostname with an optional port".into(),
            ));
        }
        Ok(())
    }
}

impl fmt::Display for SipProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.name.fmt(f)
    }
}

impl PartialEq for SipProfile {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    pub id: Uuid,
    pub name: String,
    pub address: String,
    pub profile_id: Uuid,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaperSize {
    #[default]
    A4,
    Letter,
}

impl PaperSize {
    pub const ALL: [Self; 2] = [Self::A4, Self::Letter];

    pub(crate) fn height_inches(self) -> f64 {
        match self {
            Self::A4 => 297.0 / 25.4,
            Self::Letter => 11.0,
        }
    }
}

impl fmt::Display for PaperSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::A4 => "A4",
            Self::Letter => "Letter",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    Standard,
    #[default]
    Fine,
}

impl Resolution {
    pub const ALL: [Self; 2] = [Self::Standard, Self::Fine];

    pub fn vertical_dpi(self) -> u32 {
        match self {
            Self::Standard => 98,
            Self::Fine => 196,
        }
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Standard => "Standard",
            Self::Fine => "Fine",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Binarization {
    #[default]
    Photo,
    Text,
}

impl Binarization {
    pub const ALL: [Self; 2] = [Self::Photo, Self::Text];
}

impl fmt::Display for Binarization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text => "Text",
            Self::Photo => "Photo",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentOptions {
    pub paper: PaperSize,
    pub resolution: Resolution,
    pub binarization: Binarization,
    /// Contrast adjustment in percent, from -100 to 100. Zero preserves source tones.
    #[serde(default)]
    pub contrast: i16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedDocument {
    pub id: Uuid,
    pub pages: u32,
    pub source_names: Vec<String>,
    pub options: DocumentOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaxRequest {
    pub profile_id: Uuid,
    pub destination: String,
    pub mode: FaxMode,
    pub document: PreparedDocument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Queued,
    Connecting,
    Negotiating,
    Sending {
        acknowledged_pages: u32,
    },
    Succeeded {
        acknowledged_pages: u32,
    },
    Failed {
        reason: String,
        acknowledged_pages: u32,
    },
    Cancelled {
        acknowledged_pages: u32,
    },
    Interrupted,
}

impl JobState {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Negotiating | Self::Sending { .. }
        )
    }

    pub fn is_finished(&self) -> bool {
        !matches!(self, Self::Queued) && !self.is_active()
    }

    pub fn acknowledged_pages(&self) -> u32 {
        match self {
            Self::Sending { acknowledged_pages }
            | Self::Succeeded { acknowledged_pages }
            | Self::Failed {
                acknowledged_pages, ..
            }
            | Self::Cancelled { acknowledged_pages } => *acknowledged_pages,
            _ => 0,
        }
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Queued => f.write_str("Queued"),
            Self::Connecting => f.write_str("Connecting"),
            Self::Negotiating => f.write_str("Negotiating fax"),
            Self::Sending { acknowledged_pages } => {
                write!(f, "Sending · {acknowledged_pages} acknowledged")
            }
            Self::Succeeded { acknowledged_pages } => {
                write!(f, "Delivered · {acknowledged_pages} pages")
            }
            Self::Failed { reason, .. } => write!(f, "Failed · {reason}"),
            Self::Cancelled { .. } => f.write_str("Cancelled"),
            Self::Interrupted => f.write_str("Interrupted · retry manually"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub request: FaxRequest,
    pub profile: SipProfile,
    pub state: JobState,
    pub retry_of: Option<Uuid>,
    #[serde(default)]
    pub steps: Vec<JobStep>,
    /// Outgoing UDPTL wire bytes; this does not imply acknowledged delivery.
    #[serde(default)]
    pub transmitted_bytes: u64,
    #[serde(default)]
    pub page_progress: Option<crate::PageProgress>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStep {
    pub at: DateTime<Utc>,
    pub stage: crate::FaxStage,
}

#[derive(Debug, Clone)]
pub struct DocumentInput {
    pub paths: Vec<PathBuf>,
    pub options: DocumentOptions,
}

pub(crate) fn nonempty(label: &str, value: &str) -> Result<()> {
    match value.trim().is_empty() {
        true => Err(Error::Invalid(format!("{label} is required"))),
        false => Ok(()),
    }
}

pub(crate) fn validate_sip_uri(value: &str) -> Result<()> {
    if !value.starts_with("sip:") && !value.starts_with("sips:") {
        return Err(Error::Invalid(
            "SIP URI must start with sip: or sips:".into(),
        ));
    }
    if value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '<' | '>'))
    {
        return Err(Error::Invalid(
            "SIP URI contains whitespace or angle brackets".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_destination(value: &str) -> Result<()> {
    nonempty("Destination", value)?;
    match value.starts_with("sip:") || value.starts_with("sips:") {
        true => validate_sip_uri(value),
        false
            if value
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '+' | '*' | '#')) =>
        {
            Ok(())
        }
        false => Err(Error::Invalid(
            "Use a fax number or a complete sip: URI".into(),
        )),
    }
}
