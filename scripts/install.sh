#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> Building rematch (release)..."
if [[ -f "$ROOT/Cargo.lock" ]]; then
  cargo build --release --locked
else
  cargo build --release
fi

# Provisional agents (kind `agent`) are symlinks in $REMATCH_ROOT/agents/; omp discovers
# task agents in <plugin root>/agents/, so the plugin root links there.
REMATCH_DIR="${REMATCH_ROOT:-${HOME}/.rematch}"
mkdir -p "$REMATCH_DIR/agents"
if [[ -e "$ROOT/extension/agents" && ! -L "$ROOT/extension/agents" ]]; then
  echo "error: $ROOT/extension/agents exists and is not a symlink; move it away and re-run" >&2
  exit 1
fi
ln -sfn "$REMATCH_DIR/agents" "$ROOT/extension/agents"
echo "==> Linked $ROOT/extension/agents -> $REMATCH_DIR/agents (provisional agents)"

echo "==> Linking omp plugin from $ROOT/extension..."
omp plugin link "$ROOT/extension" --scope user

BIN_DIR="${HOME}/.local/bin"
SHIM="${BIN_DIR}/rematch"
mkdir -p "$BIN_DIR"

cat > "$SHIM" <<EOF
#!/bin/sh
exec "$ROOT/target/release/rematch" "\$@"
EOF
chmod 0755 "$SHIM"
echo "==> Installed shim at $SHIM"

if [[ -x "$SHIM" ]]; then
  echo "==> Running rematch doctor..."
  "$SHIM" doctor || true
fi

cat <<'EOF'

==> Optional: configure dedicated model roles in ~/.omp/agent/config.yml:

modelRoles:
  rematch-editor: <provider>/<strongest-model-id>
  rematch-classifier: <provider>/<fast-model-id>

(If omitted, rematch-editor falls back to slow -> default, and rematch-classifier falls back to smol -> tiny -> default.)
EOF
