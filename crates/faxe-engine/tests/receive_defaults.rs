use faxe_engine::{Credentials, Engine, FaxMode, Password, ReceiveSettings, Store, Uuid};
use std::sync::Arc;

struct NoCredentials;
impl Credentials for NoCredentials {
    fn password(&self, _: Uuid) -> faxe_engine::Result<Option<Password>> {
        Ok(None)
    }
}

#[test]
fn new_and_unset_receive_folders_use_desktop_documents_without_overriding_custom_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let expected = directories::UserDirs::new()
        .map(|dirs| dirs.document_dir().unwrap_or(dirs.home_dir()).to_owned())
        .unwrap_or(directory.path().canonicalize()?);
    {
        let mut store = Store::open(directory.path())?;
        let settings = store.receive_settings()?;
        assert_eq!(settings.folder, Some(expected.clone()));
        assert_eq!(settings.profile, None);
        // Existing installs may have saved options but no receive folder.
        store.save_receive_settings(&ReceiveSettings {
            mode: FaxMode::G711,
            ecm: false,
            ..Default::default()
        })?;
    }
    let custom = directory.path().join("custom");
    {
        let mut store = Store::open(directory.path())?;
        let mut settings = store.receive_settings()?;
        assert_eq!(settings.folder, Some(expected));
        assert_eq!(settings.mode, FaxMode::G711);
        assert!(!settings.ecm);
        settings.folder = Some(custom.clone());
        store.save_receive_settings(&settings)?;
    }
    // An unavailable custom destination must not silently redirect received faxes.
    let engine = Engine::open(directory.path().to_owned(), Arc::new(NoCredentials), None)?;
    assert!(!custom.exists());
    assert_eq!(
        engine.snapshot()?.receive_settings.folder,
        Some(custom.clone())
    );
    engine.configure_receiver(ReceiveSettings {
        notifications: false,
        ..Default::default()
    })?;
    let settings = engine.snapshot()?.receive_settings;
    assert_eq!(settings.folder, Some(custom));
    assert!(!settings.notifications);
    assert_eq!(settings.profile, None);
    Ok(())
}
