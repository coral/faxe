#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

usage() {
  cat <<'HELP'
Usage: ./release.sh <patch|minor|major|release|X.Y.Z> [--execute]

Without --execute, cargo-release previews the version bump, commit, tag and push.
With --execute, it asks for confirmation, then pushes master and vX.Y.Z to origin.
GitHub Actions builds all packages and publishes the release after they succeed.
Use 'release' to tag the current version for the first release.

Prerequisites:
  cargo install cargo-release --version 0.25.20 --locked
  cargo install cargo-about --version 0.9.2 --locked --features cli
Stable versions only: Windows MSIX requires a numeric version.
HELP
}
if [[ $# == 0 || ${1:-} == --help || ${1:-} == -h ]]; then
  usage
  exit 0
fi
level=$1
shift
case "$level" in
  patch|minor|major|release) ;;
  *)
    if [[ ! $level =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
      echo "Expected patch, minor, major, release, or a stable SemVer such as 0.2.0." >&2
      exit 2
    fi
    ;;
esac
if [[ $# == 1 && $1 == --execute ]]; then
  if [[ $(cargo-about --version 2>/dev/null || true) != 'cargo-about 0.9.2' ]]; then
    echo 'Install cargo-about: cargo install cargo-about --version 0.9.2 --locked --features cli' >&2
    exit 1
  fi
elif [[ $# != 0 ]]; then
  usage >&2
  exit 2
fi
if ! cargo release --version >/dev/null 2>&1; then
  echo 'Install cargo-release: cargo install cargo-release --version 0.25.20 --locked' >&2
  exit 1
fi
exec cargo release "$level" --workspace --isolated --config release.toml "$@"
