#!/usr/bin/env bash
# Pre-push check: fmt + tests. Run before every git push.
#
# Install (one-time):
#   bash scripts/pre-push.sh --install
#
# Skip for one push:
#   git push --no-verify
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"

if [[ "${1:-}" == "--install" ]]; then
    HOOK="$REPO_ROOT/.git/hooks/pre-push"
    cp "$REPO_ROOT/scripts/pre-push.sh" "$HOOK"
    chmod +x "$HOOK"
    echo "Installed pre-push hook at $HOOK"
    exit 0
fi

cd "$REPO_ROOT"

# Git hooks don't inherit the user's PATH — load cargo explicitly.
if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
else
    export PATH="$HOME/.cargo/bin:$PATH"
fi

echo "[pre-push] Checking formatting..."
cargo fmt --all -- --check

echo "[pre-push] Running tests..."
cargo test --workspace

echo "[pre-push] All checks passed."
