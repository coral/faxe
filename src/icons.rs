//! Icons are compiled into the executable, including portable Windows builds.
pub fn window() -> iced::window::Icon {
    iced::window::icon::from_rgba(
        include_bytes!(concat!(env!("OUT_DIR"), "/window-icon.rgba")).to_vec(),
        64,
        64,
    )
    .expect("build-validated window icon")
}

#[cfg(not(target_os = "linux"))]
pub fn tray() -> Result<tray_icon::Icon, tray_icon::BadIcon> {
    let side = if cfg!(target_os = "macos") { 36 } else { 32 };
    tray_icon::Icon::from_rgba(
        include_bytes!(concat!(env!("OUT_DIR"), "/tray-icon.rgba")).to_vec(),
        side,
        side,
    )
}
