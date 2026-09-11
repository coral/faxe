use crate::{Error, FaxMode, Result, SipProfile, Uuid};
use chrono::{DateTime, Utc};
pub use faxe_native::{
    ReceiveCompletion, ReceiveOutputError, ReceivePage, ReceivePageKind, ReceiveReport,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type ProfileId = Uuid;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReceiveSettings {
    pub profile: Option<ProfileId>,
    pub listener: Option<crate::ListenerConfig>,
    /// Deliver offers through EngineRuntime::take_incoming instead of answering automatically.
    pub manual: bool,
    pub folder: Option<PathBuf>,
    pub mode: FaxMode,
    pub ecm: bool,
    pub notifications: bool,
}
impl Default for ReceiveSettings {
    fn default() -> Self {
        Self {
            profile: None,
            listener: None,
            manual: false,
            folder: None,
            mode: FaxMode::Auto,
            ecm: true,
            notifications: true,
        }
    }
}
impl ReceiveSettings {
    pub fn validate(&self, profiles: &[SipProfile]) -> Result<()> {
        if let Some(listener) = &self.listener {
            listener
                .validate()
                .map_err(|e| Error::Invalid(e.to_string()))?;
            if self.profile.is_some() {
                return Err(Error::Invalid(
                    "Choose a profile or a direct listener".into(),
                ));
            }
        }
        if let Some(id) = self.profile {
            let profile = profiles
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| Error::NotFound(id.to_string()))?;
            profile.validate()?;
            if !profile.register {
                return Err(Error::Invalid(
                    "Enable SIP registration before enabling receiving".into(),
                ));
            }
        }
        if self.profile.is_some() || self.listener.is_some() {
            if self.folder.as_ref().is_none_or(|path| !path.is_absolute()) {
                return Err(Error::Invalid(
                    "Receive folder must be an absolute path".into(),
                ));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiverState {
    #[default]
    Off,
    Registering,
    Ready,
    Receiving,
    Error {
        reason: String,
    },
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiverStatus {
    pub profile: Option<ProfileId>,
    pub state: ReceiverState,
}
impl std::fmt::Display for ReceiverStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.state {
            ReceiverState::Off => f.write_str("Receive off"),
            ReceiverState::Registering => f.write_str("Registering"),
            ReceiverState::Ready => f.write_str("Ready"),
            ReceiverState::Receiving => f.write_str("Receiving"),
            ReceiverState::Error { reason } => write!(f, "Error · {reason}"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceptionOutcome {
    Receiving,
    Received,
    Partial { reason: String },
    Failed { reason: String },
    Interrupted,
}
impl ReceptionOutcome {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Receiving)
    }
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Received)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportStatus {
    Pending,
    /// The destination and same-directory temporary file are journalled before publication.
    Publishing {
        destination: PathBuf,
        staging: PathBuf,
        bytes: u64,
        #[serde(default)]
        error: Option<String>,
    },
    Published {
        path: PathBuf,
    },
    Failed {
        reason: String,
    },
    NoContent,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceptionResult {
    Succeeded,
    Failed {
        reason: String,
        t30_code: Option<i32>,
    },
    Interrupted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedFax {
    pub id: Uuid,
    pub profile_id: Option<ProfileId>,
    pub profile_name: String,
    pub caller: String,
    #[serde(default)]
    pub destination: String,
    #[serde(default)]
    pub peer: String,
    #[serde(default)]
    pub tiff_path: Option<PathBuf>,
    pub arrived_at: DateTime<Utc>,
    /// Freeze local wall time at arrival, even if time zone changes before export.
    pub filename_stem: String,
    pub finished_at: Option<DateTime<Utc>>,
    pub options: ReceiveSettings,
    pub confirmed_pages: u32,
    pub recovered_pages: u32,
    pub transport: Option<FaxMode>,
    pub outcome: ReceptionOutcome,
    #[serde(default)]
    pub result: Option<ReceptionResult>,
    #[serde(default)]
    pub recovery: Option<ReceiveReport>,
    pub export: ExportStatus,
    /// Old records predate the opt-in preservation policy.
    pub partial_page_preservation_available: bool,
}

/// An offer expires unless accepted or rejected using its engine handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingFax {
    pub id: Uuid,
    pub caller: String,
    pub destination: String,
    pub peer: String,
    pub arrived_at: DateTime<Utc>,
}
impl ReceivedFax {
    /// Transfer and the first export attempt have both finished.
    pub fn is_finished(&self) -> bool {
        !self.outcome.is_active()
            && matches!(
                self.export,
                ExportStatus::Published { .. }
                    | ExportStatus::Failed { .. }
                    | ExportStatus::NoContent
                    | ExportStatus::Publishing { error: Some(_), .. }
            )
    }
}
