#!/usr/bin/env bash
set -euo pipefail
binding_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$binding_dir/../.." && pwd)"
prefix="${1:-$binding_dir/native}"
mkdir -p "$prefix/lib/pkgconfig" "$prefix/include"
prefix="$(cd "$prefix" && pwd)"
cd "$repo_dir"
cargo build -p faxe-ffi --release --locked
target_dir="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
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
Version: 1.0.4
Libs: -L\${libdir} -lfaxe -Wl,-rpath,\${libdir}
Cflags: -I\${includedir}
PC
printf 'Built %s\nUse: export PKG_CONFIG_PATH="%s/lib/pkgconfig:${PKG_CONFIG_PATH:-}"\n' "$library" "$prefix"
