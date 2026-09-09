#!/usr/bin/env bash
set -euo pipefail
source scripts/ci/common.sh

cargo bundle --release --format appimage --target "$CARGO_BUILD_TARGET"
appdirs=("$build"/bundle/appimage/*/AppDir)
[[ ${#appdirs[@]} == 1 ]]
appdir=${appdirs[0]}
bash scripts/ci/install-linux-icons.sh "$appdir/usr"
# Explicit desktop/icon inputs keep AppImage integration on the same artwork.
cp packaging/linux/com.coral.faxe.desktop "$appdir/usr/share/applications/faxe.desktop"
cp assets/icons/faxe-256.png "$appdir/com.coral.faxe.png"
if [[ -L "$appdir/.DirIcon" ]]; then unlink "$appdir/.DirIcon"; fi
cp assets/icons/faxe-256.png "$appdir/.DirIcon"
# Replace cargo-bundle's bare symlink with linuxdeploy's library-aware launcher.
unlink "$appdir/AppRun"
cp "$build/faxe-cli" "$appdir/usr/bin/"
mkdir -p "$appdir/usr/lib"
cp "$pdfium/lib/libpdfium.so" "$appdir/usr/lib/"
copy_notices "$appdir/usr/share/doc/faxe"
# Retain the native distribution's copyright notices as well as Cargo notices.
for copyright in /usr/share/doc/lib*/copyright /usr/share/doc/gtk*/copyright; do
  package=$(basename "$(dirname "$copyright")")
  mkdir -p "$appdir/usr/share/doc/faxe/licenses/system/$package"
  cp "$copyright" "$appdir/usr/share/doc/faxe/licenses/system/$package/"
done
case "$PACKAGE_ARCH" in
  x86_64) digest=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d ;;
  aarch64) digest=620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff ;;
  *) exit 1 ;;
esac
tool="$build/linuxdeploy.AppImage"
curl --fail --location --retry 3 \
  "https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-$PACKAGE_ARCH.AppImage" -o "$tool"
echo "$digest  $tool" | sha256sum --check
chmod +x "$tool"
export APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$PACKAGE_ARCH"
export OUTPUT="$root/dist/FAXE-$version-linux-$PACKAGE_ARCH.AppImage"
"$tool" --appdir "$appdir" --executable "$appdir/usr/bin/faxe" \
  --desktop-file "$appdir/usr/share/applications/faxe.desktop" \
  --icon-file "$appdir/usr/share/icons/hicolor/256x256/apps/com.coral.faxe.png" \
  --executable "$appdir/usr/bin/faxe-cli" --library "$appdir/usr/lib/libpdfium.so" --output appimage
test -s "$OUTPUT"
