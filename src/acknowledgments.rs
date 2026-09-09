use std::process::Command;

const URL: &str = "https://faxe.oblique.media/licenses";

pub fn open_page() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let opener = "xdg-open";

    let status = Command::new(opener)
        .arg(URL)
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("Could not open acknowledgments in the default browser".into())
    }
}
