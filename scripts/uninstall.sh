#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHIM="${HOME}/.local/bin/omp-evolve"
RELEASE_BIN="${ROOT}/target/release/omp-evolve"

if command -v omp-evolve >/dev/null 2>&1; then
  omp-evolve uninstall --yes || true
elif [[ -x "$SHIM" ]]; then
  "$SHIM" uninstall --yes || true
elif [[ -x "$RELEASE_BIN" ]]; then
  "$RELEASE_BIN" uninstall --yes || true
fi

if command -v omp >/dev/null 2>&1; then
  omp plugin uninstall @vncp/omp-evolve --scope user || omp plugin uninstall "$ROOT/extension" --scope user || true
fi

if [[ -f "$SHIM" ]]; then
  rm -f "$SHIM"
  echo "==> Removed $SHIM"
fi
