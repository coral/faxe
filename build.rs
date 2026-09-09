use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/icons");
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icons = root.join("assets/icons");
    let target = env::var("CARGO_CFG_TARGET_OS").unwrap();

    // Decode at build time: no icon filesystem reads or PNG decoding on the UI thread.
    for (source, name, size) in [
        ("faxe-64.png", "window-icon.rgba", 64),
        (
            if target == "macos" {
                "menubar/18pt/FaxeMenuBarTemplate@2x.png"
            } else {
                "faxe-32.png"
            },
            "tray-icon.rgba",
            if target == "macos" { 36 } else { 32 },
        ),
    ] {
        let image = image::open(icons.join(source))
            .expect("read application icon")
            .into_rgba8();
        assert_eq!(
            image.dimensions(),
            (size, size),
            "unexpected icon dimensions: {source}"
        );
        fs::write(out.join(name), image.as_raw()).expect("write embedded icon");
    }

    if target == "windows" {
        // MSIX needs exact logical sizes; keep color artwork and transparency.
        let assets = out.join("msix-assets");
        fs::create_dir_all(&assets).unwrap();
        let original = image::open(icons.join("faxe-1024.png")).unwrap();
        for (name, size) in [
            ("Square44x44Logo.png", 44),
            ("Square150x150Logo.png", 150),
            ("StoreLogo.png", 50),
        ] {
            for scale in [1, 2, 4] {
                let filename = if scale == 1 {
                    name.to_owned()
                } else {
                    format!(
                        "{}.scale-{}.png",
                        name.trim_end_matches(".png"),
                        scale * 100
                    )
                };
                original
                    .resize_exact(
                        size * scale,
                        size * scale,
                        image::imageops::FilterType::Lanczos3,
                    )
                    .save(assets.join(filename))
                    .unwrap();
            }
        }
        // The portable EXE needs its own icon resource, independently of MSIX.
        assert_eq!(
            env::var("CARGO_CFG_TARGET_ENV").unwrap(),
            "msvc",
            "Windows packaging requires MSVC"
        );
        let resource = out.join("faxe.rc");
        let ico = icons
            .join("faxe.ico")
            .to_string_lossy()
            .replace('\\', "\\\\");
        fs::write(&resource, format!("1 ICON \"{ico}\"\n")).unwrap();
        let compiled = out.join("faxe.res");
        let status = Command::new("rc.exe")
            .arg("/nologo")
            .arg("/fo")
            .arg(&compiled)
            .arg(&resource)
            .status()
            .expect("Windows SDK rc.exe must be on PATH");
        assert!(status.success(), "Windows icon resource compilation failed");
        println!("cargo:rustc-link-arg-bin=faxe={}", compiled.display());
    }
}
