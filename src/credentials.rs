use faxe_engine::{Credentials, Error, Password, Result, Uuid};

pub struct Keychain;

impl Credentials for Keychain {
    fn save_password(&self, profile: Uuid, password: &Password) -> Result<()> {
        save(profile, password.expose())
    }
    fn password(&self, profile: Uuid) -> Result<Option<Password>> {
        match entry(profile)?.get_password() {
            Ok(value) => Ok(Some(Password::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(Error::Credentials(error.to_string())),
        }
    }
}

pub fn save(profile: Uuid, password: &str) -> Result<()> {
    entry(profile)?
        .set_password(password)
        .map_err(|error| Error::Credentials(error.to_string()))
}

fn entry(profile: Uuid) -> Result<keyring::Entry> {
    keyring::Entry::new("com.coral.faxe.sip", &profile.to_string())
        .map_err(|error| Error::Credentials(error.to_string()))
}

#[derive(Clone)]
pub struct SecretEdit(pub String);

impl std::fmt::Debug for SecretEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}
