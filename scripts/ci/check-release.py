"""Reject mismatched tags before starting the packaging matrix."""
import os
from pathlib import Path
import re
import tomllib


def release_version():
    version = tomllib.loads(Path('Cargo.toml').read_text())['workspace']['package']['version']
    if not re.fullmatch(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', version):
        raise SystemExit(f'Releases require stable SemVer, got {version!r}')
    if any(int(part) > 65535 for part in version.split('.')):
        raise SystemExit('Windows MSIX version components must not exceed 65535')
    if os.environ['GITHUB_REF_NAME'] != f'v{version}':
        raise SystemExit('Release tag does not match the Cargo workspace version')
    return version


if __name__ == '__main__':
    print(release_version())
