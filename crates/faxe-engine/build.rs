use sha2::{Digest, Sha256};
use std::{env, fs, io::Cursor, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FAXE_PDFIUM_ARCHIVE");
    let (platform, digest) = match env::var("TARGET").unwrap().as_str() {
        "aarch64-apple-darwin" => (
            "mac-arm64",
            "61424884d4a7f153b808deba6437848e4400834ce30aaf95d3050da44df8f420",
        ),
        "x86_64-apple-darwin" => (
            "mac-x64",
            "a93d44238e05de20028446561b951d50988b849efbbe56fe40c0d376c05b45e8",
        ),
        "x86_64-unknown-linux-gnu" => (
            "linux-x64",
            "eb142f416aed3a72fc5a02dbd5884868a16cb99dc0cf53e6bdd64afbf67b05f4",
        ),
        "aarch64-unknown-linux-gnu" => (
            "linux-arm64",
            "e98400ef5f005f27cfba5c14f72d464e25187298f04950de46646033cf24cef0",
        ),
        "x86_64-pc-windows-gnu" | "x86_64-pc-windows-msvc" => (
            "win-x64",
            "78a17d9a5f14467631c26a3ac8741b27a0471ecc05bd6a119b523598160a0537",
        ),
        "aarch64-pc-windows-msvc" => (
            "win-arm64",
            "6c9ac0ddc69edd8a18d47b95098a5b843eaed5c5bbdcb9587a18c196457449f8",
        ),
        target => panic!("no PDFium artifact configured for {target}"),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let destination = out.join("pdfium-8044");
    let marker = destination.join(".verified");
    if fs::read_to_string(&marker).ok().as_deref() != Some(digest) {
        let url = format!(
            "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/8044/pdfium-{platform}.tgz"
        );
        println!("cargo:warning=Provisioning PDFium 8044 ({platform})");
        let bytes = if let Some(archive) = env::var_os("FAXE_PDFIUM_ARCHIVE") {
            println!(
                "cargo:rerun-if-changed={}",
                PathBuf::from(&archive).display()
            );
            fs::read(archive).expect("read provided PDFium archive")
        } else {
            ureq::get(&url)
                .call()
                .expect("download PDFium")
                .body_mut()
                .with_config()
                .limit(128 * 1024 * 1024)
                .read_to_vec()
                .expect("read PDFium archive")
        };
        let actual: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(actual, digest, "PDFium checksum mismatch");
        fs::create_dir_all(&destination).unwrap();
        tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new(bytes)))
            .unpack(&destination)
            .expect("extract PDFium");
        fs::write(marker, digest).unwrap();
    }
    let library_dir = destination.join(match platform.starts_with("win") {
        true => "bin",
        false => "lib",
    });
    println!("cargo:rustc-env=FAXE_PDFIUM_DIR={}", library_dir.display());
}
