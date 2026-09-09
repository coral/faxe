use faxe_engine::{Error, Password, Result, Uuid};
use windows::{
    ApplicationModel::Package,
    Security::Credentials::{PasswordCredential, PasswordVault},
    core::HSTRING,
};

const RESOURCE: &str = "com.coral.faxe.sip";

pub(super) fn packaged() -> Result<bool> {
    match Package::Current() {
        Ok(_) => Ok(true),
        Err(error) if error.code().0 as u32 == 0x80073d54 => Ok(false),
        Err(error) => Err(failure(error)),
    }
}

pub(super) fn save(profile: Uuid, password: &str) -> Result<()> {
    let credential = PasswordCredential::CreatePasswordCredential(
        &HSTRING::from(RESOURCE),
        &HSTRING::from(profile.to_string()),
        &HSTRING::from(password),
    )
    .map_err(failure)?;
    PasswordVault::new()
        .and_then(|vault| vault.Add(&credential))
        .map_err(failure)
}

pub(super) fn load(profile: Uuid) -> Result<Option<Password>> {
    let vault = PasswordVault::new().map_err(failure)?;
    let credential = match vault.Retrieve(
        &HSTRING::from(RESOURCE),
        &HSTRING::from(profile.to_string()),
    ) {
        Ok(credential) => credential,
        // HRESULT_FROM_WIN32(ERROR_NOT_FOUND): no saved password for this profile.
        Err(error) if error.code().0 as u32 == 0x80070490 => return Ok(None),
        Err(error) => return Err(failure(error)),
    };
    credential.RetrievePassword().map_err(failure)?;
    let password = credential.Password().map_err(failure)?;
    Ok(Some(Password::new(password.to_string_lossy())))
}

fn failure(error: windows::core::Error) -> Error {
    Error::Credentials(format!("Windows PasswordVault: {error}"))
}
