#[cfg(not(target_os = "linux"))]
use std::cell::RefCell;
use tray_icon::menu::{MenuEvent, MenuId};
#[cfg(not(target_os = "linux"))]
use tray_icon::{
    TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuItem},
};

#[cfg(not(target_os = "linux"))]
thread_local! {
    static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Show,
    Quit,
}

pub fn install() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    return Err("Tray support is not yet wired to a GTK event loop on Linux".into());

    #[cfg(not(target_os = "linux"))]
    {
        let menu = Menu::new();
        menu.append(&MenuItem::with_id("show", "Open Faxe", true, None))
            .map_err(|e| e.to_string())?;
        menu.append(&MenuItem::with_id("quit", "Quit Faxe", true, None))
            .map_err(|e| e.to_string())?;
        let icon = crate::icons::tray().map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("Faxe")
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_icon_as_template(cfg!(target_os = "macos"))
            .build()
            .map_err(|e| e.to_string())?;
        TRAY.with(|slot| *slot.borrow_mut() = Some(tray));
        Ok(())
    }
}

pub fn events() -> impl iced::futures::Stream<Item = Action> {
    let (sender, receiver) = iced::futures::channel::mpsc::unbounded();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = match event.id {
            id if id == MenuId::new("show") => Some(Action::Show),
            id if id == MenuId::new("quit") => Some(Action::Quit),
            _ => None,
        };
        if let Some(action) = action {
            let _ = sender.unbounded_send(action);
        }
    }));
    receiver
}
