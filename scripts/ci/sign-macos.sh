#!/usr/bin/env bash
set -euo pipefail
set +x
umask 077

for name in APPLE_CERTIFICATE_BASE64 APPLE_CERTIFICATE_PASSWORD APPLE_API_KEY_BASE64 APPLE_API_KEY_ID APPLE_API_ISSUER_ID; do
  if [[ -z "${!name:-}" ]]; then echo "Missing release secret: $name" >&2; exit 1; fi
done
: "${RUNNER_TEMP:?This script runs on the GitHub macOS signing runner}"
work="$RUNNER_TEMP/faxe-signing"
mkdir -p "$work" dist
keychain="$work/signing.keychain-db"
mountpoint="$work/mounted"
mounted=false
cleanup() {
  if [[ "$mounted" == true ]]; then hdiutil detach "$mountpoint" >/dev/null 2>&1 || true; fi
  security delete-keychain "$keychain" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

shopt -s nullglob
archives=(unsigned/*.dmg)
if [[ ${#archives[@]} != 1 ]]; then echo 'Expected one macOS disk image' >&2; exit 1; fi
mkdir -p "$mountpoint" "$work/app"
hdiutil attach -readonly -nobrowse -mountpoint "$mountpoint" "${archives[0]}"
mounted=true
apps=("$mountpoint"/*.app)
if [[ ${#apps[@]} != 1 ]]; then echo 'Expected one macOS application' >&2; exit 1; fi
app="$work/app/$(basename "${apps[0]}")"
ditto "${apps[0]}" "$app"
hdiutil detach "$mountpoint"
mounted=false

printf '%s' "$APPLE_CERTIFICATE_BASE64" | base64 --decode > "$work/certificate.p12"
keychain_password=$(openssl rand -hex 32)
echo "::add-mask::$keychain_password"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 2700 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$work/certificate.p12" -k "$keychain" -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" >/dev/null
security list-keychains -d user -s "$keychain"
rm "$work/certificate.p12"
unset APPLE_CERTIFICATE_BASE64 APPLE_CERTIFICATE_PASSWORD keychain_password

identity=$(security find-identity -v -p codesigning "$keychain" | awk '/"Developer ID Application:/ {print $2}')
if [[ ! "$identity" =~ ^[0-9A-Fa-f]{40}$ ]]; then
  echo 'Expected one valid Developer ID Application identity in the certificate export' >&2
  exit 1
fi
for binary in "$app"/Contents/Frameworks/*.dylib "$app"/Contents/MacOS/*; do
  codesign --force --sign "$identity" --keychain "$keychain" --options runtime --timestamp "$binary"
done
codesign --force --sign "$identity" --keychain "$keychain" --options runtime --timestamp "$app"
codesign --verify --deep --strict "$app"
printf '%s' "$APPLE_API_KEY_BASE64" | base64 --decode > "$work/notary-key.p8"
xcrun notarytool store-credentials faxe-notarization --keychain "$keychain" \
  --key "$work/notary-key.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" >/dev/null
rm "$work/notary-key.p8"
unset APPLE_API_KEY_BASE64 APPLE_API_KEY_ID APPLE_API_ISSUER_ID
# Notarizing the outer disk image also notarizes the application it contains.
dmg="dist/$(basename "${archives[0]}")"
bash scripts/ci/create-macos-dmg.sh "$app" "$dmg"
codesign --force --sign "$identity" --keychain "$keychain" --timestamp "$dmg"
codesign --verify --strict "$dmg"
xcrun notarytool submit "$dmg" --keychain "$keychain" \
  --keychain-profile faxe-notarization --wait --timeout 30m --output-format json > "$work/result.json"
if [[ $(jq -r .status "$work/result.json") != Accepted ]]; then
  submission=$(jq -r .id "$work/result.json")
  xcrun notarytool log "$submission" --keychain "$keychain" \
    --keychain-profile faxe-notarization "$work/notary-log.json"
  jq '.issues' "$work/notary-log.json"
  echo 'Apple did not accept the notarization submission' >&2
  exit 1
fi
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
