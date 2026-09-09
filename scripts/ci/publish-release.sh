#!/usr/bin/env bash
set -euo pipefail
: "${GITHUB_REF_NAME:?Expected a version tag}"

# Enumerate expected artifacts explicitly. Never ship the unsigned macOS input.
python3 - <<'PY'
from pathlib import Path
import runpy
import shutil

version = runpy.run_path('scripts/ci/check-release.py')['release_version']()
expected = [
    ('faxe-aarch64-apple-darwin', f'FAXE-{version}-macos-arm64.dmg'),
    ('faxe-source', 'faxe-source.tar.gz'),
    ('faxe-windows-bundle', f'FAXE-{version}-windows-unsigned.msixbundle'),
]
for arch, target in [('x64', 'x86_64'), ('arm64', 'aarch64')]:
    expected.append((f'faxe-{target}-pc-windows-msvc', f'FAXE-{version}-windows-{arch}-portable.zip'))
for arch in ['x86_64', 'aarch64']:
    expected.extend([
        (f'faxe-{arch}-unknown-linux-gnu', f'FAXE-{version}-linux-{arch}.AppImage'),
        (f'faxe-flatpak-{arch}', f'FAXE-linux-{arch}.flatpak'),
    ])
paths = [Path('artifacts') / artifact / filename for artifact, filename in expected]
for path in paths:
    if not path.is_file() or path.stat().st_size == 0:
        raise SystemExit(f'Missing or empty release asset: {path}')
destination = Path('release-assets')
destination.mkdir()
for path in paths:
    shutil.copy2(path, destination / path.name)
PY
(
  cd release-assets
  sha256sum -- * > SHA256SUMS
)
# A failed upload leaves a draft, never an incomplete public release.
if gh release view "$GITHUB_REF_NAME" --json isDraft --jq .isDraft > release-is-draft; then
  if [[ $(cat release-is-draft) != true ]]; then
    echo "Release $GITHUB_REF_NAME is already public; refusing to replace its assets." >&2
    exit 1
  fi
else
  gh release create "$GITHUB_REF_NAME" --verify-tag --draft --generate-notes \
    --title "FAXE ${GITHUB_REF_NAME#v}"
fi
gh release upload "$GITHUB_REF_NAME" release-assets/* --clobber
gh release edit "$GITHUB_REF_NAME" --draft=false --latest
