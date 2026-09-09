#!/usr/bin/env bash
set -euo pipefail
source scripts/ci/common.sh

cargo bundle --release --format osx --target "$CARGO_BUILD_TARGET"
apps=("$build"/bundle/osx/*.app)
[[ ${#apps[@]} == 1 ]]
app=${apps[0]}
# Preserve the supplied multi-resolution ICNS exactly, before signing the bundle.
cp assets/icons/faxe.icns "$app/Contents/Resources/faxe.icns"
/usr/libexec/PlistBuddy -c 'Set :CFBundleIconFile faxe.icns' "$app/Contents/Info.plist"
cmp assets/icons/faxe.icns "$app/Contents/Resources/faxe.icns"
frameworks="$app/Contents/Frameworks"
mkdir -p "$frameworks"
cp "$build/faxe-cli" "$app/Contents/MacOS/"
cp "$pdfium/lib/libpdfium.dylib" "$frameworks/"
chmod u+w "$frameworks/libpdfium.dylib"
install_name_tool -id '@loader_path/libpdfium.dylib' "$frameworks/libpdfium.dylib"
copy_notices "$app/Contents/Resources"

# Recursively copy each Homebrew dylib and repair its references. Keep a queue
# instead of --deep signing, so every library is processed and signed explicitly.
queue=("$app/Contents/MacOS/faxe" "$app/Contents/MacOS/faxe-cli" "$frameworks/libpdfium.dylib")
originals=("$build/faxe" "$build/faxe-cli" "$pdfium/lib/libpdfium.dylib")
index=0
while [[ $index -lt ${#queue[@]} ]]; do
  binary=${queue[$index]}
  original_binary=${originals[$index]}
  install_id=$(otool -D "$original_binary" | tail -n +2)
  index=$((index + 1))
  while IFS= read -r dependency; do
    if [[ "$dependency" == "$install_id" ]]; then continue; fi
    case "$dependency" in
      /System/*|/usr/lib/*) continue ;;
    esac
    resolved=$dependency
    case "$dependency" in
      @loader_path/*) resolved="$(dirname "$original_binary")/${dependency#@loader_path/}" ;;
      @rpath/*)
        resolved=''
        while IFS= read -r rpath; do
          rpath=${rpath/@loader_path/$(dirname "$original_binary")}
          rpath=${rpath/@executable_path/$build}
          candidate="$rpath/${dependency#@rpath/}"
          if [[ -f "$candidate" ]]; then resolved=$candidate; break; fi
        done < <(otool -l "$original_binary" | awk '/cmd LC_RPATH/ { found=1; next } found && $1 == "path" { sub(/^ *path /, ""); sub(/ \(offset.*$/, ""); print; found=0 }')
        ;;
    esac
    case "$resolved" in
      /opt/homebrew/*|/usr/local/*) ;;
      *) echo "Unresolved library $dependency in $binary" >&2; exit 1 ;;
    esac
    name=$(basename "$dependency")
    target="$frameworks/$name"
    if [[ ! -f "$target" ]]; then
      cp -L "$resolved" "$target"
      chmod u+w "$target"
      install_name_tool -id "@loader_path/$name" "$target"
      queue+=("$target")
      originals+=("$resolved")
      # The real path identifies the formula and version whose notices to retain.
      original=$(realpath "$resolved")
      prefix=${original%%/lib/*}
      formula=$(basename "$(dirname "$prefix")")
      destination="$app/Contents/Resources/licenses/homebrew/$formula/$(basename "$prefix")"
      mkdir -p "$destination"
      notices=("$prefix"/LICENSE* "$prefix"/COPYING* "$prefix"/COPYRIGHT*)
      if [[ ${#notices[@]} == 0 ]]; then
        echo "No license notices found for $resolved" >&2; exit 1
      fi
      cp "${notices[@]}" "$destination/"
      if [[ -d "$prefix/share/doc" ]]; then cp -R "$prefix/share/doc" "$destination/"; fi
    fi
    if [[ "$binary" == "$frameworks/"* ]]; then
      replacement="@loader_path/$name"
    else
      replacement="@loader_path/../Frameworks/$name"
    fi
    install_name_tool -change "$dependency" "$replacement" "$binary"
  done < <(otool -L "$original_binary" | tail -n +2 | sed -E 's/^[[:space:]]*//; s/ \(compatibility.*$//')
done
for binary in "${queue[@]}"; do
  if otool -L "$binary" | tail -n +2 | grep -Eq '/opt/|/Users/'; then
    echo "Build-host library path remains in $binary" >&2; exit 1
  fi
  codesign --force --sign - "$binary"
done
# Ad-hoc signing is required for ARM64 execution; no Developer ID is used here.
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
bash scripts/ci/create-macos-dmg.sh "$app" "$root/dist/FAXE-$version-macos-arm64.dmg"
