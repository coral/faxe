use crate::{Error, Result};
pub use faxe_native::{CallLimits, ListenerConfig, SignalingTransport};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EngineOptions {
    pub limits: CallLimits,
    pub document_workers: usize,
    pub admission_timeout_secs: u64,
}
impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            limits: CallLimits::default(),
            document_workers: 1,
            admission_timeout_secs: 5,
        }
    }
}
impl EngineOptions {
    pub fn server() -> Self {
        Self {
            limits: CallLimits::SERVER,
            document_workers: 2,
            ..Self::default()
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.limits
            .validate()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if self.document_workers == 0 || self.admission_timeout_secs == 0 {
            return Err(Error::Invalid(
                "Worker count and admission timeout must be positive".into(),
            ));
        }
        Ok(())
    }
}
