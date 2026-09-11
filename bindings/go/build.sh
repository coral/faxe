#!/usr/bin/env bash
set -euo pipefail
binding_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$binding_dir/../.." && pwd)"
skip_build=false
if [ "${1:-}" = "--skip-build" ]; then
  # CI builds the Rust targets together before staging the library for Go.
  skip_build=true
  shift
fi
prefix="${1:-$binding_dir/native}"
mkdir -p "$prefix/lib/pkgconfig" "$prefix/include"
prefix="$(cd "$prefix" && pwd)"
cd "$repo_dir"
if [ "$skip_build" = false ]; then
  cargo build -p faxe-ffi --release --locked
fi
cargo_metadata="$(cargo metadata --locked --no-deps --format-version 1)"
target_dir="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' <<< "$cargo_metadata")"
version="$(python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "faxe-ffi"))' <<< "$cargo_metadata")"
if [ -n "${CARGO_BUILD_TARGET:-}" ]; then target_dir="$target_dir/$CARGO_BUILD_TARGET"; fi
case "$(uname -s)" in
  Darwin) library=libfaxe.dylib ;;
  Linux) library=libfaxe.so ;;
  *) echo 'The Go binding supports Linux and macOS.' >&2; exit 1 ;;
esac
cp "$target_dir/release/$library" "$prefix/lib/$library"
cp crates/faxe-ffi/include/faxe.h "$prefix/include/faxe.h"
cat > "$prefix/lib/pkgconfig/faxe.pc" <<PC
prefix=$prefix
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: faxe
Description: Embedded Faxe engine (C ABI 1)
Version: $version
Libs: -L\${libdir} -lfaxe -Wl,-rpath,\${libdir}
Cflags: -I\${includedir}
PC
printf 'Built %s\nUse: export PKG_CONFIG_PATH="%s/lib/pkgconfig:${PKG_CONFIG_PATH:-}"\n' "$library" "$prefix"
