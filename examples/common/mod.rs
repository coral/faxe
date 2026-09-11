use faxe_engine::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub type AnyError = Box<dyn std::error::Error + Send + Sync>;

struct EnvironmentCredentials;
impl Credentials for EnvironmentCredentials {
    fn password(&self, profile: Uuid) -> Result<Option<Password>> {
        let key = format!("FAXE_SIP_PASSWORD_{}", profile.simple());
        match std::env::var(key).or_else(|_| std::env::var("FAXE_SIP_PASSWORD")) {
            Ok(password) => Ok(Some(Password::new(password))),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(Error::Credentials("Password must be valid Unicode".into())),
        }
    }
}

pub fn logging() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "faxe=info".into()),
        )
        .init();
}

pub fn profile(path: &Path) -> std::result::Result<SipProfile, AnyError> {
    let profile: SipProfile = serde_json::from_slice(&std::fs::read(path)?)?;
    profile.validate()?;
    Ok(profile)
}

pub async fn open(
    directory: PathBuf,
    concurrency: usize,
) -> std::result::Result<EngineRuntime, AnyError> {
    let directory = std::path::absolute(directory)?;
    Ok(tokio::task::spawn_blocking(move || {
        let options = EngineOptions {
            limits: CallLimits {
                outgoing: concurrency,
                incoming: concurrency,
            },
            ..EngineOptions::server()
        };
        let backend = Arc::new(SipSender::with_options(&options)?);
        EngineRuntime::open_with_options(
            directory,
            Arc::new(EnvironmentCredentials),
            Some(backend),
            options,
        )
    })
    .await??)
}

pub async fn interrupted() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = terminate.recv() => () }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
