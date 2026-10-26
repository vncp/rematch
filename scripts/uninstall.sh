#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHIM="${HOME}/.local/bin/rematch"
RELEASE_BIN="${ROOT}/target/release/rematch"

if command -v rematch >/dev/null 2>&1; then
  rematch uninstall --yes || true
elif [[ -x "$SHIM" ]]; then
  "$SHIM" uninstall --yes || true
elif [[ -x "$RELEASE_BIN" ]]; then
  "$RELEASE_BIN" uninstall --yes || true
fi

if command -v omp >/dev/null 2>&1; then
  omp plugin uninstall @vncp/rematch --scope user || omp plugin uninstall "$ROOT/extension" --scope user || true
fi

if [[ -f "$SHIM" ]]; then
  rm -f "$SHIM"
  echo "==> Removed $SHIM"
fi
