use std::{collections::BTreeSet, env, fs, path::Path, process::Command};

const LIBRARIES: &[(&str, &str)] = &[
    ("pjsip", "pjsip-ua"),
    ("pjsip", "pjsip"),
    ("pjmedia", "pjmedia-codec"),
    ("pjmedia", "pjmedia"),
    ("pjnath", "pjnath"),
    ("pjlib-util", "pjlib-util"),
    ("pjlib", "pjlib"),
];

fn main() {
    println!("cargo:rerun-if-env-changed=FAXE_BUILD_PATH");
    for path in ["build.rs", "wrapper.h", "native", "pjproject"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = std::path::PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let original = Path::new(&manifest).join("pjproject");
    assert!(
        original.join("version.mak").exists(),
        "PJPROJECT is missing; initialize checkout submodules with git submodule update --init --recursive"
    );
    let source = out.join("source");
    copy_source(&original, &source);
    fs::write(
        source.join("pjlib/include/pj/config_site.h"),
        "#define PJ_HAS_SSL_SOCK 0\n#define PJMEDIA_HAS_VIDEO 0\n#define PJMEDIA_HAS_SRTP 0\n#define PJ_IOQUEUE_MAX_HANDLES 2048\n",
    )
    .unwrap();
    let build = out.join("build");
    let compiler = cc::Build::new().get_compiler();
    let cpp_compiler = cc::Build::new().cpp(true).get_compiler();
    let mut configure = Command::new("cmake");
    match env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "linux" => {
            configure.arg("-DPJLIB_WITH_IOQUEUE=epoll");
        }
        "macos" => {
            configure.arg("-DPJLIB_WITH_IOQUEUE=kqueue");
        }
        _ => (),
    }
    configure
        .args(["-S", &format!("{manifest}/native"), "-B"])
        .arg(&build)
        .args(["-G", "Ninja"])
        .arg(format!("-DPJ_SOURCE={}", source.display()))
        .arg(format!("-DCMAKE_C_COMPILER={}", compiler.path().display()))
        .arg(format!(
            "-DCMAKE_CXX_COMPILER={}",
            cpp_compiler.path().display()
        ))
        .args([
            "-DCMAKE_BUILD_TYPE=Release",
            "-DCMAKE_POSITION_INDEPENDENT_CODE=ON",
            "-DBUILD_SHARED_LIBS=OFF",
            "-DBUILD_TESTING=OFF",
            "-DPJ_SKIP_EXPERIMENTAL_NOTICE=ON",
            "-DPJLIB_WITH_SSL=",
            "-DPJMEDIA_WITH_RESAMPLE=none",
        ]);
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" && compiler.is_like_clang() {
        let target = env::var("TARGET").unwrap();
        configure.arg(format!("-DCMAKE_C_COMPILER_TARGET={target}"));
        configure.arg(format!("-DCMAKE_CXX_COMPILER_TARGET={target}"));
    }
    for feature in [
        "PJSIP_WITH_TLS",
        "PJNATH_WITH_UPNP",
        "PJMEDIA_WITH_SRTP",
        "PJMEDIA_WITH_VIDEO",
        "PJMEDIA_WITH_AUDIODEV",
        "PJMEDIA_WITH_FFMPEG",
        "PJMEDIA_WITH_LIBYUV",
        "PJMEDIA_WITH_SPEEX_AEC",
        "PJMEDIA_WITH_WEBRTC_AEC",
        "PJMEDIA_WITH_WEBRTC_AEC3",
        "PJMEDIA_WITH_GSM_CODEC",
        "PJMEDIA_WITH_SPEEX_CODEC",
        "PJMEDIA_WITH_ILBC_CODEC",
        "PJMEDIA_WITH_G722_CODEC",
        "PJMEDIA_WITH_G7221_CODEC",
        "PJMEDIA_WITH_OPENCORE_AMRNB_CODEC",
        "PJMEDIA_WITH_OPENCORE_AMRWB_CODEC",
        "PJMEDIA_WITH_OPUS_CODEC",
        "PJMEDIA_WITH_BCG729_CODEC",
        "PJMEDIA_WITH_SILK_CODEC",
        "PJMEDIA_WITH_LYRA_CODEC",
    ] {
        configure.arg(format!("-D{feature}=OFF"));
    }
    run(&mut configure);
    run(Command::new("cmake")
        .arg("--build")
        .arg(&build)
        .args(["--target", "faxe-libraries", "--parallel"])
        .arg(env::var("NUM_JOBS").unwrap_or_else(|_| "2".into())));
    let mut arguments = BTreeSet::new();
    for (directory, library) in LIBRARIES {
        println!(
            "cargo:rustc-link-search=native={}",
            build.join("pj").join(directory).display()
        );
        println!("cargo:rustc-link-lib=static={library}");
        for include in fs::read_to_string(build.join(format!("{library}.includes")))
            .unwrap()
            .lines()
            .filter(|s| !s.is_empty())
        {
            arguments.insert(format!("-I{include}"));
        }
        for define in fs::read_to_string(build.join(format!("{library}.defines")))
            .unwrap()
            .lines()
            .filter(|s| !s.is_empty())
        {
            arguments.insert(format!("-D{define}"));
        }
    }
    match env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "macos" => {
            println!("cargo:rustc-link-lib=framework=Foundation");
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
        }
        "linux" => {
            for library in ["pthread", "m", "rt", "uuid"] {
                println!("cargo:rustc-link-lib={library}");
            }
        }
        "windows" => {
            for library in ["ws2_32", "iphlpapi", "ole32", "uuid", "winmm"] {
                println!("cargo:rustc-link-lib={library}");
            }
        }
        target => panic!("unsupported desktop target: {target}"),
    }
    bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(arguments)
        .clang_arg(format!("--target={}", env::var("TARGET").unwrap()))
        .allowlist_function("pj.*")
        .allowlist_type("pj.*")
        .allowlist_var("(PJ|pj).*")
        .derive_default(true)
        .generate_comments(false)
        .layout_tests(false)
        .generate()
        .expect("generate PJPROJECT bindings")
        .write_to_file(out.join("bindings.rs"))
        .unwrap();
}

fn run(command: &mut Command) {
    if let Some(path) = env::var_os("FAXE_BUILD_PATH") {
        command.env("PATH", path);
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    assert!(
        output.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn copy_source(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".git" {
            continue;
        }
        let target = destination.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy_source(&entry.path(), &target),
            false => {
                let content = fs::read(entry.path()).unwrap();
                if fs::read(&target).ok().as_deref() != Some(content.as_slice()) {
                    fs::write(target, content).unwrap();
                }
            }
        }
    }
}
