use clap::{Parser, Subcommand, ValueEnum};
use faxe_engine::{
    Binarization, Cancellation, Credentials, DocumentInput, DocumentOptions, Documents, Engine,
    Error, FaxMode, FaxRequest, PaperSize, Password, Resolution, Result, SipProfile, SipSender,
    Uuid,
};
use std::{path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(about = "Headless Faxe document preparation and queue management")]
struct Arguments {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Prepare {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(long)]
        letter: bool,
        #[arg(long)]
        standard: bool,
        /// Use photo processing (the default).
        #[arg(long, conflicts_with = "text")]
        photo: bool,
        /// Use text processing instead of photo processing.
        #[arg(long)]
        text: bool,
    },
    Profiles,
    ImportProfile {
        file: PathBuf,
    },
    Jobs,
    Inbox,
    ReceiveSettings,
    /// Configure receiving; only `run` registers and answers calls.
    Receive {
        #[arg(long, conflicts_with = "disable")]
        profile: Option<Uuid>,
        #[arg(long)]
        disable: bool,
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long, value_enum)]
        mode: Option<Mode>,
        #[arg(long)]
        ecm: Option<bool>,
        #[arg(long)]
        notifications: Option<bool>,
    },
    RetryExport {
        fax: Uuid,
    },
    /// Process queued faxes until interrupted with Ctrl-C.
    Run,
    Queue {
        #[arg(long)]
        profile: Uuid,
        #[arg(long)]
        document: Uuid,
        destination: String,
        /// Override the selected profile's sending mode for this fax.
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    Cancel {
        job: Uuid,
    },
    Retry {
        job: Uuid,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Auto,
    T38,
    G711,
}

struct EnvironmentCredentials;

impl Credentials for EnvironmentCredentials {
    fn password(&self, profile: Uuid) -> Result<Option<Password>> {
        let key = format!("FAXE_SIP_PASSWORD_{}", profile.simple());
        match std::env::var(key) {
            Ok(value) => Ok(Some(Password::new(value))),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(Error::Credentials(
                "SIP password environment variable is not valid Unicode".into(),
            )),
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "faxe=info".into()),
        )
        .init();
    match run(Arguments::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(arguments: Arguments) -> Result<()> {
    let directory = match arguments.data_dir {
        Some(path) => path,
        None => Engine::data_directory()?,
    };
    let documents = Documents::new(directory.join("spool"))?;
    let backend = match arguments.command {
        Command::Run => Some(Arc::new(SipSender::new()?) as Arc<dyn faxe_engine::FaxBackend>),
        _ => None,
    };
    let mut engine = Engine::open(directory, Arc::new(EnvironmentCredentials), backend)?;
    match arguments.command {
        Command::Run => {
            let mut events = engine.subscribe();
            loop {
                tokio::select! {
                    signal = tokio::signal::ctrl_c() => { signal?; break; }
                    event = events.recv() => match event {
                        Ok(faxe_engine::EngineEvent::JobChanged(job)) => println!("{}", serde_json::to_string(&*job)?),
                        Ok(faxe_engine::EngineEvent::ReceivedFaxChanged(fax)) => println!("{}", serde_json::to_string(&*fax)?),
                        Ok(faxe_engine::EngineEvent::ReceiverStatus(status)) => println!("{}", serde_json::to_string(&status)?),
                        Ok(faxe_engine::EngineEvent::Error(error)) => eprintln!("{error}"),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => println!("{}", serde_json::to_string(&engine.snapshot()?.jobs)?),
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err(Error::WorkerStopped),
                    }
                }
            }
        }
        Command::Inbox => println!(
            "{}",
            serde_json::to_string_pretty(&engine.snapshot()?.inbox)?
        ),
        Command::ReceiveSettings => println!(
            "{}",
            serde_json::to_string_pretty(&engine.snapshot()?.receive_settings)?
        ),
        Command::Receive {
            profile,
            disable,
            folder,
            mode,
            ecm,
            notifications,
        } => {
            let mut settings = engine.snapshot()?.receive_settings;
            if disable {
                settings.profile = None;
            } else if profile.is_some() {
                settings.profile = profile;
            }
            if let Some(folder) = folder {
                settings.folder = Some(std::path::absolute(folder)?);
            }
            if let Some(mode) = mode {
                settings.mode = match mode {
                    Mode::Auto => FaxMode::Auto,
                    Mode::G711 => FaxMode::G711,
                    Mode::T38 => FaxMode::T38,
                };
            }
            if let Some(ecm) = ecm {
                settings.ecm = ecm;
            }
            if let Some(notifications) = notifications {
                settings.notifications = notifications;
            }
            engine.configure_receiver(settings)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&engine.snapshot()?.receive_settings)?
            );
        }
        Command::RetryExport { fax } => {
            let mut events = engine.subscribe();
            let record = engine
                .snapshot()?
                .inbox
                .into_iter()
                .find(|record| record.id == fax)
                .ok_or_else(|| Error::NotFound(fax.to_string()))?;
            if matches!(
                record.export,
                faxe_engine::ExportStatus::Published { .. } | faxe_engine::ExportStatus::NoContent
            ) {
                println!("{}", serde_json::to_string_pretty(&record)?);
            } else {
                engine.retry_export(fax)?;
                loop {
                    match events.recv().await {
                        Ok(faxe_engine::EngineEvent::ReceivedFaxChanged(record))
                            if record.id == fax =>
                        {
                            match &record.export {
                                faxe_engine::ExportStatus::Published { .. }
                                | faxe_engine::ExportStatus::NoContent => {
                                    println!("{}", serde_json::to_string_pretty(&*record)?);
                                    break;
                                }
                                faxe_engine::ExportStatus::Failed { reason }
                                | faxe_engine::ExportStatus::Publishing {
                                    error: Some(reason),
                                    ..
                                } => return Err(Error::Invalid(reason.clone())),
                                _ => (),
                            }
                        }
                        Err(_) => return Err(Error::WorkerStopped),
                        _ => (),
                    }
                }
            }
        }
        Command::Prepare {
            files,
            letter,
            standard,
            photo: _,
            text,
        } => {
            let options = DocumentOptions {
                paper: match letter {
                    true => PaperSize::Letter,
                    false => PaperSize::A4,
                },
                resolution: match standard {
                    true => Resolution::Standard,
                    false => Resolution::Fine,
                },
                binarization: match text {
                    true => Binarization::Text,
                    false => Binarization::Photo,
                },
                ..DocumentOptions::default()
            };
            let document = documents.prepare(
                DocumentInput {
                    paths: files,
                    options,
                },
                &Cancellation::default(),
                |pages| eprintln!("Prepared {pages} page(s)"),
            )?;
            println!("{}", serde_json::to_string_pretty(&document)?);
            eprintln!("TIFF: {}", documents.fax_path(document.id).display());
        }
        Command::Profiles => println!(
            "{}",
            serde_json::to_string_pretty(&engine.snapshot()?.profiles)?
        ),
        Command::ImportProfile { file } => {
            let profile: SipProfile = serde_json::from_slice(&std::fs::read(file)?)?;
            engine.save_profile(&profile)?;
            println!("{}", profile.id);
        }
        Command::Jobs => println!(
            "{}",
            serde_json::to_string_pretty(&engine.snapshot()?.jobs)?
        ),
        Command::Queue {
            profile,
            document,
            destination,
            mode,
        } => {
            let job = engine.enqueue(FaxRequest {
                profile_id: profile,
                document: documents.load(document)?,
                destination,
                mode: match mode {
                    Some(Mode::Auto) => FaxMode::Auto,
                    Some(Mode::T38) => FaxMode::T38,
                    Some(Mode::G711) => FaxMode::G711,
                    None => {
                        engine
                            .snapshot()?
                            .profiles
                            .iter()
                            .find(|candidate| candidate.id == profile)
                            .ok_or_else(|| Error::NotFound(profile.to_string()))?
                            .sending_mode
                    }
                },
            })?;
            println!("{}", serde_json::to_string_pretty(&job)?);
            eprintln!("Queued. Start the desktop or use `faxe-cli run` to process the queue.");
        }
        Command::Cancel { job } => engine.cancel(job)?,
        Command::Retry { job } => {
            println!("{}", serde_json::to_string_pretty(&engine.retry(job)?)?)
        }
    }
    engine.shutdown()
}
