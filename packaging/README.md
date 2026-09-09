# CI and packaging

`.github/workflows/build.yml` builds on pushes, pull requests and manual dispatch.
It uses stable Rust, the committed Cargo.lock, and native GitHub-hosted runners.
Builds use Cargo; packaging uses cargo-bundle, shell/PowerShell, the Windows SDK,
and flatpak-builder.

| Runner | Target | Downloadable artifact |
| --- | --- | --- |
| `macos-15` | macOS arm64, Sequoia 15+ | `.app` in a DMG |
| `windows-2025` | Windows x64 MSVC | portable ZIP and unsigned MSIX |
| `windows-11-arm` | Windows arm64 MSVC | portable ZIP and unsigned MSIX |
| `windows-2025` | Windows x64 + arm64 bundle | unsigned MSIX bundle |
| `ubuntu-24.04` | Linux x86_64 | AppImage and Flatpak |
| `ubuntu-24.04-arm` | Linux aarch64 | AppImage and Flatpak |

Find downloads under **Actions → Build and package → the run → Artifacts**.
Artifacts are retained for 14 days. Pushes and manual runs on `master` also sign
and notarize the macOS app. Version tags also publish permanent downloads under **Releases** after every
package succeeds. Successful version-tag releases then submit the Windows bundle
to Microsoft Store using the `ms` environment.
Download the matching `faxe-source` artifact when redistributing binaries;
retain it alongside those binaries after the Actions artifacts expire.

`faxe-source` includes the repository, initialized PJPROJECT sources, vendored
Cargo dependencies (including SpanDSP's C sources), and build scripts. The
native dependency setup is pinned in the repository. CI actions are pinned to
commit IDs. Only the final GitHub release job has repository write permission.

## Desktop builds

Native prerequisites are installed per runner: CMake, Ninja, Clang/libclang,
pkg-config, TIFF and JPEG; Linux also installs GTK/AppIndicator and AppImage
tools. Windows imports the native Visual Studio build environment, uses Clang
with the MSVC ABI, and builds the pinned vcpkg manifest in `windows/vcpkg.json`.
No SpanDSP dependency sources or build scripts are patched.

Windows packaging follows normal and delay-load PE imports with LLVM's
`llvm-readobj`, starting from both executables and the dynamically loaded PDFium
DLL. Only required vcpkg and MSVC runtime DLLs are copied; unknown non-system
imports fail packaging. Each run uses a fresh staging directory so obsolete DLLs
cannot leak into the ZIP or MSIX.

The build command is `cargo build --workspace --release --locked` with the
matrix target in `CARGO_BUILD_TARGET`. `scripts/ci/` then packages those outputs.
The desktop and headless CLI are included.
On Windows, first run `./scripts/ci/setup-windows.ps1 -Local` as described in
the root README. After setup, ordinary local development starts with `cargo run`.

macOS uses cargo-bundle 0.11.0, collects non-system Homebrew dylibs into
`Contents/Frameworks`, fixes their library paths and ad-hoc signs the result.
The compressed DMG contains `FAXE.app` and an Applications shortcut for drag-to-install.
The build artifact is named `faxe-macos-unsigned`. A separate macOS job on
`master` or a `v*` tag downloads that run's artifact, signs the app and its libraries with
Developer ID and hardened runtime, signs the DMG, notarizes it with Apple
(including the app inside), staples the ticket to the DMG,
and uploads `faxe-aarch64-apple-darwin`. Pull requests and other branches only
produce the build artifact. `macos/cargo.toml` remains an optional local overlay.

The signing job uses the GitHub `release` environment, restricted to the
`master` branch and `v*` tags, with five environment secrets:

- `APPLE_CERTIFICATE_BASE64`: base64-encoded Developer ID Application `.p12`.
- `APPLE_CERTIFICATE_PASSWORD`: the `.p12` export password.
- `APPLE_API_KEY_BASE64`: base64-encoded App Store Connect team API key (`.p8`).
- `APPLE_API_KEY_ID`: the API key ID.
- `APPLE_API_ISSUER_ID`: the API issuer ID.

Notarization uses the API key; no Apple account password is supplied to CI.
No provisioning profile or Mac App
Store record is required for the current direct-download build. Signing material
is imported into a temporary keychain after the build, never cached or uploaded,
and removed on exit with an additional always-run cleanup step. Build jobs do
not receive these secrets. Restrict write access to `master` and its workflows:
code allowed to run in the signing job can access its credentials.

Linux uses cargo-bundle to create its AppDir, then a checksum-verified
linuxdeploy release collects shared libraries and creates the final AppImage.
Builds on Ubuntu 24.04 require a compatible host libc; they do not claim support
for older Linux distributions. Native notices are copied into the package.

PDFium is pinned to chromium/8044, including Windows ARM64. Packaged binaries
load PDFium from the application bundle; development builds retain the embedded
runtime fallback. `FAXE_PDFIUM_ARCHIVE` optionally supplies a local archive to
the build script, which checks the same target-specific SHA-256 digest.

## Windows identity and signing

Windows Store packages use `packagedClassicApp` / `appContainer`. Their only
capabilities are `internetClientServer` (Internet SIP/media traffic) and
`privateNetworkClientServer` (LAN SIP servers and peers). They do not declare
`runFullTrust` or other restricted capabilities. Packaging and bundling both
check each application and its capabilities, so an old full-trust package
cannot be mixed into the x64/arm64 bundle.

### Local AppContainer testing (Windows x64)

After `scripts/ci/setup-windows.ps1 -Local`, with Windows Developer Mode enabled:

```powershell
./scripts/test-appcontainer.ps1 -Launch
```

This builds a debug package, validates it with MakeAppx, registers the separate
`com.coral.faxe.AppContainerTest` identity, and opens `FAXE AppContainer Test`.
Close the previous test instance before rebuilding. Each build has a newer
test version so Windows uses the new staging directory. Build output and the
latest staging path are in `build/appcontainer/`. Keep the registered staging
directory in place while using the app. The generated unsigned MSIX is a local
test artifact; registration uses the loose development manifest.

The test uses the same AppContainer manifest and capabilities as the Store
package, with a separate identity and development version. See
[Microsoft's AppContainer packaging guidance](https://learn.microsoft.com/en-us/windows/msix/msix-container).

Packaged builds now use `ApplicationData.LocalFolder` for config, database,
spool and cache. The `FAXE_APP_DIR` override still takes precedence. Unpackaged
builds retain their desktop paths. Existing desktop profiles are not imported
into the test identity. Avoid canonicalizing package-local storage through
ancestors the sandbox cannot access.

Locally verified: x64 build, MakeAppx validation, package registration, process
token `TokenIsAppContainer=1`, UI rendering, and engine startup after fixing
the storage path and receive-folder fallback. The classic document picker
failed with access denied when browsing Desktop, so packaged builds now use
`Windows.Storage.Pickers.FileOpenPicker` and copy selected `StorageFile` objects
through the broker to package-local cache. Imported copies currently remain in
`LocalCache/import-*` until the test data is removed. A local document import
and one-page preparation subsequently succeeded. The desktop credential
backend also failed with access denied; packaged builds now use Windows
`PasswordVault`, while unpackaged builds retain the existing keyring backend.
Credential saving and a real outgoing fax were also verified locally. Receiving,
credential reload after restart, notifications, tray and opening exported files
still need functional verification. Receive
folder selection still uses the classic picker, and opening exports currently
starts Explorer; both need further AppContainer work.

PasswordVault gives AppContainer apps their own credential locker and has a
[20-credential limit per app](https://learn.microsoft.com/en-us/uwp/api/windows.security.credentials.passwordvault.add).
Existing desktop credentials are not automatically migrated.

User-visible operation errors are also written to stderr, even with
`RUST_LOG=off`. Windows console output omits ANSI escapes. For a console launch, capture them with
`cargo run 2>&1 | Tee-Object faxe-debug.log`. Launching via the Start menu does
not attach stderr to an existing terminal.

Remove the local test registration when finished (also removes its app data):

```powershell
Get-AppxPackage -Name com.coral.faxe.AppContainerTest | Remove-AppxPackage
```

With no repository variables configured, MSIX packages use the explicit test
identity `com.coral.faxe.CI` / `CN=FAXE CI`. They are unsigned CI artifacts, not
Store submissions or directly installable signed packages. The portable ZIP
can be used independently of MSIX signing.

To render MSIX with the actual Partner Center identity, set all three GitHub
repository variables:

- `FAXE_STORE_IDENTITY`: Package/Identity/Name.
- `FAXE_STORE_PUBLISHER`: Package/Identity/Publisher.
- `FAXE_STORE_DISPLAY_NAME`: publisher display name.

The package and application display name in the manifest matches the reserved
Store name `FAXE ( FAX over SIP )`.

Partial configuration fails the packaging job. Values are XML-escaped when
rendering `windows/AppxManifest.xml.in`. The version is the Cargo application
version plus `.0`. Manifest validation runs through MakeAppx. Signing and Store
submission remain separate; no certificate or private key is required by CI.

After the desktop matrix succeeds, the `windows-bundle` job downloads both
Windows artifacts and runs `scripts/ci/bundle-windows.ps1`. It checks that there
is exactly one package per architecture with matching identities and versions
and that both packages use AppContainer with only the two network capabilities,
then uses [MakeAppx bundle](https://learn.microsoft.com/en-us/windows/msix/packaging-tool/bundle-msix-packages)
to create `FAXE-<version>-windows-unsigned.msixbundle`. The bundle version is
explicitly set to the package version (`<version>.0`), and its manifest is checked
for both architectures before upload as the `faxe-windows-bundle` artifact.
The individual MSIX files remain available in the architecture build artifacts.

For a Store submission, configure the three Partner Center identity variables
above before building, then upload the `.msixbundle` from `faxe-windows-bundle`
to Partner Center's Packages page. Microsoft signs packages distributed through
the Store; CI leaves this submission bundle unsigned. An unsigned bundle using
the fallback CI identity is only a test artifact. Direct installation outside
the Store requires signing the bundle separately.

SpanDSP 0.2.3 supplies the Windows static-library build fix. CI uses native
Actions runners for Windows and Linux compilation and packaging.

### Automated Microsoft Store updates

`.github/workflows/microsoft-store.yml` uses the GitHub environment **`ms`**:

- `AZURE_AD_TENANT_ID`: the associated Microsoft Entra tenant ID.
- `AZURE_AD_APPLICATION_CLIENT_ID`: the CI app registration's client ID.
- `AZURE_AD_APPLICATION_SECRET`: the client secret **value**, not its ID.
- `SELLER_ID`: numeric Seller ID from Partner Center's Legal info / Developer page.
- `PRODUCT_ID`: FAXE's Store ID, `9PK6QF9MMNGJ`.

Add the Entra application in Partner Center's User management with the Manager
role. The first submission must be published and live before automated updates
can run. Microsoft currently documents this GitHub workflow for free products.
See [Microsoft's setup instructions](https://learn.microsoft.com/en-us/windows/apps/publish/msstore-dev-cli/github-actions).

After a `vX.Y.Z` build publishes its complete GitHub release, the Store job
downloads that run's `faxe-windows-bundle` and verifies the version, Store
identity, publisher and architectures before uploading and committing it for
certification. It reuses the last published listing and availability settings;
select automatic publication after certification in Partner Center if updates
should go live without a manual release. The job waits for commit processing,
not the full certification. A green job means submitted, not certified or live.

Store jobs are serialized. An existing draft or pending certification stops
upload, because `msstore publish` can otherwise replace a draft. Resolve the
existing submission in Partner Center before rerunning the failed Store job.
If an upload or commit fails, inspect Partner Center first: a draft may remain.
Do not edit submissions in Partner Center while CI is submitting an update.

Run **Actions → Microsoft Store → Run workflow** on `master` to check credentials,
app access and current submission status without uploading or changing anything.
This also works while the first submission is in certification. The CLI and its
setup action are pinned, and credentials are reset in an always-run cleanup step.
Environment deployment rules, if enabled, must permit `v*` tags and `master`
for this read-only check; required reviewers will pause automatic updates.

## Flatpak

The manifest targets GNOME Platform/SDK 50 with the Rust stable and LLVM 21 SDK
extensions. Prepare local sources:

```sh
cargo vendor --locked --versioned-dirs .flatpak-vendor > .flatpak-cargo-config.toml
git clone https://github.com/flathub/shared-modules.git packaging/flatpak/shared-modules
git -C packaging/flatpak/shared-modules checkout cb9ec602a1ece1c76d5a4f8aa1d87c4a6bf99c3e
```

After installing the matching runtimes/extensions and flatpak-builder:

```sh
flatpak-builder --user --force-clean --disable-rofiles-fuse \
  --repo=packaging/flatpak/repo packaging/flatpak/build \
  packaging/flatpak/com.coral.faxe.json
mkdir -p dist
flatpak build-bundle --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo \
  packaging/flatpak/repo dist/FAXE.flatpak com.coral.faxe
```

Flatpak sources include a checksum-pinned PDFium archive for each architecture.
Cargo compiles offline from the vendored crates; the application build module
has no network grant. The SDK supplies its native runtime libraries. The
separately pinned AppIndicator shared module is built into the app.

The manifest permits SIP networking, desktop integration, Documents write
access and Downloads read access. Linux tray handling remains unimplemented in
FAXE; packaging does not change that application behavior. The local source
manifest and unfinished AppStream homepage/release fields still need completion
before a Flathub submission.

## Licensing

FAXE is GPL-3.0-only. Packages include the checked-in license inventory and
notices from the actual PDFium artifact and native package dependencies. The
acknowledgments hosted on the website remain generated release inputs; follow
`licenses/README.md` when dependencies change. Notices do not replace GPL/LGPL
corresponding-source obligations. Keep the source artifact available for every
binary release you redistribute.

## Creating a release

Use [cargo-release](https://github.com/crate-ci/cargo-release) through the root
`release.sh`. All five Rust packages inherit one workspace version, produce one
release commit and one annotated `vX.Y.Z` tag, and are not published to crates.io.

```sh
cargo install cargo-release --version 0.25.20 --locked
cargo install cargo-about --version 0.9.2 --locked --features cli
./release.sh release             # Preview the first tag at the current version
./release.sh release --execute   # Confirm, commit if needed, tag and push
./release.sh patch               # Preview the next patch release
./release.sh minor --execute     # Or major / an explicit version such as 0.2.0
```

Commit your changes before executing, and release from `master`. The default is
a dry run. `--execute` keeps cargo-release's confirmation and clean-tree/remote
checks. The desktop release hook regenerates license outputs after the lockfile
version bump, so the release commit contains current acknowledgments. Stable SemVer is supported; prerelease suffixes are rejected because
Windows MSIX needs a numeric version. CI checks that the tag matches Cargo.toml
and that each version component fits MSIX's numeric range before packaging.

The tag push runs the existing build matrix, including signing and notarization.
The final job requires nine nonempty assets: signed macOS DMG, two Windows ZIPs,
one unsigned MSIX bundle, two AppImages, two Flatpaks, and corresponding source. It adds
`SHA256SUMS`, uploads everything to a draft, then publishes it with generated
release notes. A missing/failed build prevents publication. An upload failure
leaves a draft; rerun the failed job to finish. Already-public releases are not
overwritten. Unsigned macOS build inputs are excluded.

The GitHub `release` environment must allow a **tag** deployment rule `v*` as
well as its existing `master` branch rule. Apple secrets remain environment
secrets. Repository writers able to push these tags can trigger signing.

## Application icons

`assets/icons` is the source for all application artwork:

- macOS bundles the supplied `faxe.icns` unchanged for Finder and the Dock. The
  menu bar embeds `menubar/18pt/FaxeMenuBarTemplate@2x.png` and marks it as an
  AppKit template; tray-icon displays it at 18 logical points.
- Windows embeds the multi-size color `faxe.ico` into `faxe.exe`, so portable
  executables have the icon in Explorer. The window and tray use embedded color
  PNG pixels. The build script generates exact MSIX logo dimensions from the
  1024-pixel color source, with 200% and 400% variants; packaging copies those
  generated assets and includes `faxe.ico` in the portable ZIP.
- AppImage and Flatpak install every supplied `hicolor` PNG size under the
  `com.coral.faxe` icon name. The AppImage desktop entry and `.DirIcon` use the
  same color artwork. Linux window icons use embedded color pixels; Linux tray
  support remains unavailable until the GTK event loop is integrated.

Icon decoding happens at build time, not on the window thread. No runtime icon
files are required alongside the executable. Windows builds require Windows SDK
`rc.exe` on PATH (the CI Visual Studio environment supplies it). The generated
Windows assets live under the desktop build script's `OUT_DIR/msix-assets`.
