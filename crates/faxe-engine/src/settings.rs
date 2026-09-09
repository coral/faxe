use crate::{DocumentOptions, Error, Result, Uuid};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub struct AppDirectories {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
}

impl AppDirectories {
    pub fn resolve() -> Result<Self> {
        if let Some(root) = std::env::var_os("FAXE_APP_DIR") {
            let root = PathBuf::from(root);
            if !root.is_absolute() {
                return Err(Error::Invalid(
                    "FAXE_APP_DIR must be an absolute path".into(),
                ));
            }
            return Ok(Self {
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
            });
        }
        let dirs = ProjectDirs::from("com", "coral", "faxe")
            .ok_or_else(|| Error::Invalid("Could not determine application directories".into()))?;
        Ok(Self {
            config: dirs.config_dir().to_owned(),
            data: dirs.data_dir().to_owned(),
            cache: dirs.cache_dir().to_owned(),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub document_options: DocumentOptions,
    pub default_profile: Option<Uuid>,
}

impl Settings {
    pub fn load() -> Result<Self> {
        Self::read(&AppDirectories::resolve()?.config)
    }

    fn read(directory: &Path) -> Result<Self> {
        match fs::read(directory.join("settings.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.write(&AppDirectories::resolve()?.config)
    }

    fn write(&self, directory: &Path) -> Result<()> {
        fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.as_file().sync_all()?;
        file.persist(directory.join("settings.json"))
            .map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_and_missing_fields_get_defaults() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path();
        assert_eq!(Settings::read(path)?.default_profile, None);
        let mut settings = Settings {
            default_profile: Some(Uuid::new_v4()),
            ..Default::default()
        };
        settings.write(path)?;
        assert_eq!(
            Settings::read(path)?.default_profile,
            settings.default_profile
        );
        settings.default_profile = Some(Uuid::new_v4());
        settings.document_options.contrast = 35;
        settings.write(path)?;
        assert_eq!(Settings::read(path)?.document_options.contrast, 35);
        assert_eq!(
            Settings::read(path)?.default_profile,
            settings.default_profile
        );
        fs::write(path.join("settings.json"), "{}")?;
        assert_eq!(Settings::read(path)?.default_profile, None);
        fs::write(
            path.join("settings.json"),
            r#"{"document_options":{"paper":"A4","resolution":"Fine","binarization":"Text"}}"#,
        )?;
        assert_eq!(Settings::read(path)?.document_options.contrast, 0);
        fs::write(path.join("settings.json"), "broken")?;
        assert!(Settings::read(path).is_err());
        Ok(())
    }
}
