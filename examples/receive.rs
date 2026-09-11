//! cargo run -p faxe-engine --example receive -- --listen 127.0.0.1:5060
mod common;
use clap::{Parser, ValueEnum};
use common::AnyError;
use faxe_engine::*;
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Clone, Copy, ValueEnum)]
enum Transport {
    Udp,
    Tcp,
}
#[derive(Parser)]
struct Arguments {
    #[arg(long, required_unless_present = "listen", conflicts_with = "listen")]
    profile: Option<PathBuf>,
    #[arg(long)]
    listen: Option<SocketAddr>,
    #[arg(long, value_enum, default_value = "udp")]
    transport: Transport,
    #[arg(long)]
    advertised_ip: Option<IpAddr>,
    #[arg(long, default_value = "./faxe-receive-data")]
    data_dir: PathBuf,
    #[arg(long, default_value = "./received-faxes")]
    output: PathBuf,
    #[arg(long, env = "CONCURRENCY", default_value_t = 100)]
    concurrency: usize,
}
#[tokio::main]
async fn main() -> std::result::Result<(), AnyError> {
    common::logging();
    let args = Arguments::parse();
    let output = std::path::absolute(args.output)?;
    tokio::fs::create_dir_all(&output).await?;
    let runtime = common::open(args.data_dir, args.concurrency).await?;
    let handle = runtime.handle();
    let mut events = handle.subscribe();
    let mut settings = ReceiveSettings {
        folder: Some(output),
        notifications: false,
        ..Default::default()
    };
    if let Some(path) = args.profile {
        let profile = common::profile(&path)?;
        settings.profile = Some(profile.id);
        handle.save_profile(profile).await?;
    } else if let Some(bind) = args.listen {
        settings.listener = Some(ListenerConfig {
            bind,
            advertised_ip: args.advertised_ip,
            transport: match args.transport {
                Transport::Udp => SignalingTransport::Udp,
                Transport::Tcp => SignalingTransport::Tcp,
            },
        });
    }
    handle.configure_receiver(settings).await?;
    loop {
        tokio::select! {
            _ = common::interrupted() => break,
            event = events.recv() => match event {
                Ok(EngineEvent::ReceivedFaxChanged(fax)) => println!("{}", serde_json::to_string(&*fax)?),
                Ok(EngineEvent::ReceiverStatus(status)) => eprintln!("{status}"),
                Ok(EngineEvent::Error(error)) => eprintln!("{error}"),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => println!("{}", serde_json::to_string(&*handle.view().inbox)?),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => (),
            }
        }
    }
    runtime.shutdown().await?;
    Ok(())
}
