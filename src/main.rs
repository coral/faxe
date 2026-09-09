mod acknowledgments;
mod credentials;
mod desktop;
mod icons;
mod tray;
#[cfg(target_os = "windows")]
mod windows_picker;

fn main() -> iced::Result {
    tracing_subscriber::fmt()
        .with_ansi(
            cfg!(not(target_os = "windows"))
                && std::io::IsTerminal::is_terminal(&std::io::stderr()),
        )
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "faxe=info".into()),
        )
        .init();
    iced::daemon(desktop::App::boot, desktop::App::update, desktop::App::view)
        .title("Faxe")
        .theme(desktop::theme())
        .subscription(desktop::App::subscription)
        .run()
}
