"""Local release checks; fake assets and gh, never contact GitHub."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / 'scripts/ci').mkdir(parents=True)
        for name in ['check-release.py', 'publish-release.sh']:
            shutil.copy2(ROOT / 'scripts/ci' / name, self.root / 'scripts/ci' / name)
        (self.root / 'Cargo.toml').write_text('[workspace.package]\nversion = "1.2.3"\n')
        assets = {
            'faxe-aarch64-apple-darwin': ['FAXE-1.2.3-macos-arm64.dmg'],
            'faxe-source': ['faxe-source.tar.gz'],
            'faxe-macos-unsigned': ['DO-NOT-PUBLISH.dmg'],
            'faxe-windows-bundle': ['FAXE-1.2.3-windows-unsigned.msixbundle'],
        }
        for arch, target in [('x64', 'x86_64'), ('arm64', 'aarch64')]:
            assets[f'faxe-{target}-pc-windows-msvc'] = [
                f'FAXE-1.2.3-windows-{arch}-{suffix}'
                for suffix in ['portable.zip', 'unsigned.msix']
            ]
        for arch in ['x86_64', 'aarch64']:
            assets[f'faxe-{arch}-unknown-linux-gnu'] = [f'FAXE-1.2.3-linux-{arch}.AppImage']
            assets[f'faxe-flatpak-{arch}'] = [f'FAXE-linux-{arch}.flatpak']
        for artifact, names in assets.items():
            directory = self.root / 'artifacts' / artifact
            directory.mkdir(parents=True)
            for name in names:
                (directory / name).write_text('fixture\n')
        bin_dir = self.root / 'bin'
        bin_dir.mkdir()
        gh = bin_dir / 'gh'
        gh.write_text('''#!/usr/bin/env bash
printf '%s\\n' "$*" >> "$GH_TEST_LOG"
case "$2" in
  view) case "${GH_TEST_STATE:-missing}" in missing) exit 1;; draft) echo true;; public) echo false;; esac;;
  upload) if [[ ${GH_TEST_FAIL_UPLOAD:-0} == 1 ]]; then exit 1; fi;;
esac
''')
        gh.chmod(0o755)
        self.env = dict(os.environ, GITHUB_REF_NAME='v1.2.3',
                        PATH=f'{bin_dir}:{os.environ["PATH"]}', GH_TEST_LOG=str(self.root / 'gh.log'))

    def run_release(self):
        return subprocess.run(['bash', 'scripts/ci/publish-release.sh'], cwd=self.root,
                              env=self.env, capture_output=True, text=True)

    def commands(self):
        path = self.root / 'gh.log'
        return path.read_text() if path.exists() else ''

    def test_complete_release_and_checksums(self):
        result = self.run_release()
        self.assertEqual(result.returncode, 0, result.stderr)
        assets = self.root / 'release-assets'
        self.assertEqual(len(list(assets.iterdir())), 10)
        self.assertFalse((assets / 'DO-NOT-PUBLISH.dmg').exists())
        self.assertTrue((assets / 'FAXE-1.2.3-windows-unsigned.msixbundle').is_file())
        self.assertEqual(list(assets.glob('*.msix')), [])
        self.assertEqual(len((assets / 'SHA256SUMS').read_text().splitlines()), 9)
        verified = subprocess.run(['sha256sum', '--check', 'SHA256SUMS'], cwd=assets, capture_output=True)
        self.assertEqual(verified.returncode, 0)
        self.assertIn('release create v1.2.3 --verify-tag --draft', self.commands())
        self.assertIn('release edit v1.2.3 --draft=false --latest', self.commands())

    def test_missing_asset_never_creates_release(self):
        (self.root / 'artifacts/faxe-source/faxe-source.tar.gz').unlink()
        self.assertNotEqual(self.run_release().returncode, 0)
        self.assertEqual(self.commands(), '')

    def test_mismatched_tag_never_creates_release(self):
        self.env['GITHUB_REF_NAME'] = 'v1.2.4'
        self.assertNotEqual(self.run_release().returncode, 0)
        self.assertEqual(self.commands(), '')

    def test_missing_or_empty_bundle_never_creates_release(self):
        bundle = self.root / 'artifacts/faxe-windows-bundle/FAXE-1.2.3-windows-unsigned.msixbundle'
        for state in ['empty', 'missing']:
            with self.subTest(state=state):
                if state == 'empty':
                    bundle.write_bytes(b'')
                else:
                    bundle.unlink()
                self.assertNotEqual(self.run_release().returncode, 0)
                self.assertEqual(self.commands(), '')

    def test_failed_upload_keeps_draft(self):
        self.env['GH_TEST_FAIL_UPLOAD'] = '1'
        self.assertNotEqual(self.run_release().returncode, 0)
        self.assertNotIn('release edit', self.commands())

    def test_retry_draft_and_protect_public_release(self):
        self.env['GH_TEST_STATE'] = 'draft'
        self.assertEqual(self.run_release().returncode, 0)
        self.assertNotIn('release create', self.commands())
        shutil.rmtree(self.root / 'release-assets')
        (self.root / 'gh.log').unlink()
        self.env['GH_TEST_STATE'] = 'public'
        self.assertNotEqual(self.run_release().returncode, 0)
        self.assertNotIn('release upload', self.commands())


if __name__ == '__main__':
    unittest.main()
