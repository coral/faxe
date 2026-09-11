# Licensing and acknowledgments

FAXE's original workspace code is licensed under **GPL-3.0-only**; see the root
`LICENSE`. Third-party sources retain their own copyright notices and licenses.
PJPROJECT's GPL-2.0-or-later option permits its use under GPLv3 in FAXE.
The SpanDSP C fax library is LGPL-2.1-only, while its Rust wrappers are MIT.

[`web/public/licenses.html`](../web/public/licenses.html) is served at
<https://faxe.oblique.media/licenses>. The **Acknowledgments** button at the
bottom of Settings opens that URL in the default browser; the HTML is no longer
embedded in the executable or written to the app cache. Opening it requires
network access. The page supports search, links each Rust dependency to its full
notices, and prints all notices even when a search filter is active.
`manifest.json` provides the same inventory in machine-readable form without
local filesystem paths. Native and supplemental notice snapshots remain here
for release packaging.

## Regenerate

Install the license-generation tool (not required for normal `cargo run`):

```sh
cargo install cargo-about --version 0.9.2 --locked --features cli
python3 scripts/generate-licenses.py
python3 scripts/generate-licenses.py --check
```

Python 3.11 or later is required. `--cargo-about /path/to/cargo-about` supports
a tool installed outside PATH. Run `cargo fetch --locked` once if the registry
cache is incomplete; generation itself runs offline with Cargo's frozen graph and
the five architecture/OS targets in `about.toml`, includes build dependencies,
and excludes dependencies used only in tests. AppImage and Flatpak share the
Linux target inventory. It neither builds nor launches FAXE. Check in both
generated outputs after dependency changes; `--check` fails for stale output.
The outputs are `licenses/manifest.json` and `web/public/licenses.html`.
Website CI runs `python3 scripts/generate-licenses.py --check-page`, which checks
the HTML against the checked-in inventory and its Cargo.lock hash without
requiring cargo-about or the native toolchain. This does not replace a full
regeneration after dependency or native-notice changes.

Cargo-about groups exact license texts, preserving differing copyright notices.
Its generic MIT fallback contains placeholder copyright holders, so the generator
replaces those entries using reviewed, checksummed `supplemental.json` records.
These copy package license files or license files at the published source
revision. Where upstream supplies only a license declaration, a record includes
that declaration, the package's author metadata, and the referenced MIT terms.
Such records explicitly document this basis; no copyright owner/year is invented.
New unresolved MIT entries or missing crate coverage stop generation for review.

## Native libraries

Cargo metadata alone cannot describe the native libraries. `native.json` records
their versions, upstream sources, notes, and hashes of the notice snapshots in
`native/`. It includes PJPROJECT, SpanDSP, PDFium's complete mac-arm64 archive
license directory, LibTIFF, libjpeg-turbo, and the current LibTIFF compression
dependencies (WebP, Zstandard and liblzma).

The native snapshot was collected from PJPROJECT revision
`5a457451fa2712ba18e12b01738e8ff3af2b26fd`, registry `spandsp-sys` 0.2.4,
the checksum-verified PDFium chromium/8044 mac-arm64 artifact, and the native
library versions listed in `native.json`. The generator checks PJPROJECT and
SpanDSP notice drift, the PDFium version, and rejects SpanDSP's `v150` feature.
FAXE explicitly requests only `fax`; do not enable `v150`, which introduces
GPLv2-only modules incompatible with this combined GPLv3 application.

For each release platform, inspect the libraries actually bundled and their
transitive native dependencies. Refresh these snapshots and versions if they
differ. Other PDFium platform archives and Linux/Windows system library builds
may carry additional notices; this macOS baseline is not a completed audit of
those future packages. OS-provided frameworks are not bundled here.

Full upstream COPYING files may discuss licenses for tools/tests that FAXE does
not link; the accompanying component descriptions identify the code used.
The native snapshots and generated notices do not relicense third-party code.

## Distribution

Ship notices and provide the corresponding source for each release, including
native modifications and build scripts. Retain dependency notices when updating
or repackaging. A license inventory is not a substitute for GPL/LGPL source and
rebuilding obligations or a review of the final distributable.
