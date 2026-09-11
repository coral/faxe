pub fn reception_finished(caller: &str, pages: u32) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // The backend's implicit default looks up an app named "use_default"
        // through AppleScript, which opens macOS's application chooser.
        // Set our bundle ID directly, once, before sending any notification.
        static APPLICATION: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        APPLICATION
            .get_or_init(|| {
                notify_rust::set_application("com.coral.faxe").map_err(|error| error.to_string())
            })
            .clone()?;
    }

    notify_rust::Notification::new()
        .summary("FAXE · reception finished")
        .body(&format!("{caller} · {pages} recovered page(s)"))
        .show()
        .map(|_| ())
        .map_err(|error| error.to_string())
}
