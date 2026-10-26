#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> Building omp-evolve (release)..."
if [[ -f "$ROOT/Cargo.lock" ]]; then
  cargo build --release --locked
else
  cargo build --release
fi

echo "==> Linking omp plugin from $ROOT/extension..."
omp plugin link "$ROOT/extension" --scope user

BIN_DIR="${HOME}/.local/bin"
SHIM="${BIN_DIR}/omp-evolve"
mkdir -p "$BIN_DIR"

cat > "$SHIM" <<EOF
#!/bin/sh
exec "$ROOT/target/release/omp-evolve" "\$@"
EOF
chmod 0755 "$SHIM"
echo "==> Installed shim at $SHIM"

if [[ -x "$SHIM" ]]; then
  echo "==> Running omp-evolve doctor..."
  "$SHIM" doctor || true
fi

cat <<'EOF'

==> Optional: configure dedicated model roles in ~/.omp/agent/config.yml:

modelRoles:
  evolve-editor: <provider>/<strongest-model-id>
  evolve-classifier: <provider>/<fast-model-id>

(If omitted, evolve-editor falls back to slow -> default, and evolve-classifier falls back to smol -> tiny -> default.)
EOF
