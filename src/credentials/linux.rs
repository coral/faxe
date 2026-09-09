use secret_service::{EncryptionType, Error, blocking::SecretService};

pub(super) fn save_password(entry: &keyring::Entry, password: &str) -> keyring::Result<()> {
    match entry.set_password(password) {
        Err(keyring::Error::NoStorageAccess(error))
            if matches!(error.downcast_ref::<Error>(), Some(Error::NoResult)) =>
        {
            // The keyring store assumes the default collection already exists.
            // Fresh desktops can expose Secret Service before a wallet is set up.
            // Recheck the alias: only its absence should trigger wallet creation.
            ensure_default_collection()
                .map_err(|error| keyring::Error::NoStorageAccess(Box::new(error)))?;
            entry.set_password(password)
        }
        result => result,
    }
}

fn ensure_default_collection() -> Result<(), Error> {
    let service = SecretService::connect(EncryptionType::Dh)?;
    match service.get_default_collection() {
        Ok(_) => Ok(()),
        Err(Error::NoResult) => {
            // Passing the alias makes creation race-safe and lets the desktop
            // show its native setup prompt. Never use the ephemeral session store.
            service.create_collection("Default", "default")?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}
