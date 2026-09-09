#!/usr/bin/env bash
set -euo pipefail

: "${CARGO_BUILD_TARGET:?Set the native Rust target}"
: "${PACKAGE_ARCH:?Set the package architecture}"
root=$(pwd)
build="$root/target/$CARGO_BUILD_TARGET/release"
version=$(cargo metadata --no-deps --format-version 1 --locked | jq -r '.packages[] | select(.name == "faxe-desktop") | .version')
mkdir -p "$root/dist"
shopt -s nullglob
pdfium_markers=("$build"/build/faxe-engine-*/out/pdfium-8044/.verified)
if [[ ${#pdfium_markers[@]} != 1 ]]; then
  echo "Expected one PDFium artifact, found ${#pdfium_markers[@]}" >&2
  exit 1
fi
pdfium=$(dirname "${pdfium_markers[0]}")

copy_notices() {
  local destination=$1
  mkdir -p "$destination/licenses/pdfium-artifact"
  cp LICENSE "$destination/"
  cp -R licenses/. "$destination/licenses/"
  cp -R "$pdfium/licenses/." "$destination/licenses/pdfium-artifact/"
  cp "$pdfium/LICENSE" "$destination/licenses/pdfium-artifact/ARCHIVE-LICENSE"
  cat > "$destination/SOURCE.txt" <<EOF
FAXE $version, revision $(git rev-parse HEAD)
GPL-3.0-only. The matching faxe-source artifact in this Actions run includes
FAXE, PJPROJECT, vendored Rust dependencies including SpanDSP, and build scripts.
Keep that corresponding source available alongside redistributed binaries.
EOF
}
