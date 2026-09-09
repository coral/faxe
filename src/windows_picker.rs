//! Brokered document access for packaged Windows apps.
use iced::{Task, window};
use raw_window_handle::RawWindowHandle;
use std::path::PathBuf;
use windows::{
    Storage::{
        ApplicationData, CreationCollisionOption, NameCollisionOption, Pickers::FileOpenPicker,
    },
    Win32::{Foundation::HWND, UI::Shell::IInitializeWithWindow},
    core::{HSTRING, Interface},
};

pub fn documents(id: window::Id) -> Task<Result<Vec<PathBuf>, String>> {
    // Create and show on the window thread; only await/copy on the executor.
    window::run(id, |window| {
        let handle = window.window_handle().map_err(|e| e.to_string())?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return Err("Expected a Windows window handle".into());
        };
        let picker = FileOpenPicker::new().map_err(|e| e.to_string())?;
        let initialize: IInitializeWithWindow = picker.cast().map_err(|e| e.to_string())?;
        // The borrowed HWND belongs to the live window executing this callback.
        unsafe { initialize.Initialize(HWND(handle.hwnd.get() as *mut _)) }
            .map_err(|e| e.to_string())?;
        let filters = picker.FileTypeFilter().map_err(|e| e.to_string())?;
        for extension in [".pdf", ".jpg", ".jpeg", ".png"] {
            filters
                .Append(&HSTRING::from(extension))
                .map_err(|e| e.to_string())?;
        }
        picker.PickMultipleFilesAsync().map_err(|e| e.to_string())
    })
    .then(|operation| {
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let files: Vec<_> = {
                        let selected = operation?.join().map_err(|e| e.to_string())?;
                        selected.into_iter().collect()
                    };
                    if files.is_empty() {
                        return Ok(Vec::new());
                    }
                    let root = ApplicationData::Current()
                        .and_then(|data| data.LocalCacheFolder())
                        .map_err(|e| e.to_string())?;
                    let folder = root
                        .CreateFolderAsync(
                            &HSTRING::from(format!("import-{}", faxe_engine::Uuid::new_v4())),
                            CreationCollisionOption::FailIfExists,
                        )
                        .map_err(|e| e.to_string())?
                        .join()
                        .map_err(|e| e.to_string())?;
                    let mut paths = Vec::new();
                    for file in files {
                        // A brokered StorageFile grants access to its contents; its original
                        // filesystem path does not grant access to ordinary Rust file APIs.
                        let name = file.Name().map_err(|e| e.to_string())?;
                        let local = file
                            .CopyOverload(&folder, &name, NameCollisionOption::GenerateUniqueName)
                            .map_err(|e| e.to_string())?
                            .join()
                            .map_err(|e| e.to_string())?;
                        paths.push(PathBuf::from(
                            local.Path().map_err(|e| e.to_string())?.to_os_string(),
                        ));
                    }
                    Ok(paths)
                })
                .await
                .map_err(|e| e.to_string())?
            },
            |result| result,
        )
    })
}
