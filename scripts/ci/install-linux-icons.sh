#!/usr/bin/env bash
set -euo pipefail
prefix=${1:?Usage: install-linux-icons.sh PREFIX [MAX_SIZE]}
max_size=${2:-0}
if [[ ! "$max_size" =~ ^[0-9]+$ ]]; then
  echo 'MAX_SIZE must be a non-negative integer (0 means unlimited)' >&2
  exit 1
fi
for source in assets/icons/hicolor/*/apps/faxe.png; do
  size=${source#assets/icons/hicolor/}
  size=${size%%/*}
  width=${size%x*}
  height=${size#*x}
  if (( max_size > 0 && (width > max_size || height > max_size) )); then
    continue
  fi
  directory="$prefix/share/icons/hicolor/$size/apps"
  mkdir -p "$directory"
  install -m644 "$source" "$directory/com.coral.faxe.png"
done
