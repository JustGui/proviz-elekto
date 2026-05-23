#!/usr/bin/env bash
# Full proviz-elekto workflow: build → seed → select (CLI) → select (HTTP)
# Run from repo root: bash examples/rust/workflow.sh
set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

BIN_DIR="$REPO_ROOT/target/release"
DB="$REPO_ROOT/workflow-demo.db"
PORT_FILE="$(mktemp)"
SERVER_PID=""

cleanup() {
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
    rm -f "$DB" "$PORT_FILE"
}
trap cleanup EXIT

echo "==> Building release binaries..."
cargo build --release --bin proviz --bin proviz-server

echo
echo "==> Seeding catalog (brands + models)..."
rm -f "$DB"
"$BIN_DIR/proviz" --db-path "$DB" seed --brands --models

echo
echo "==> Loading providers from ./providers directory (free plan)..."
"$BIN_DIR/proviz" --db-path "$DB" providers load --dir ./providers --plan free || true

echo
echo "==> Listing brands:"
"$BIN_DIR/proviz" --db-path "$DB" brand list

echo
echo "==> Listing models:"
"$BIN_DIR/proviz" --db-path "$DB" model list

echo
echo "==> Adding selection rules for step 'chat'..."
# Try Groq Llama first, fall back to Mistral
"$BIN_DIR/proviz" --db-path "$DB" rule add \
    --step chat --model llama-3.3-70b-versatile --priority 0 2>/dev/null \
    || echo "  (llama-3.3-70b-versatile not found, skipping)"
"$BIN_DIR/proviz" --db-path "$DB" rule add \
    --step chat --model mistral-small-latest --priority 1 2>/dev/null \
    || echo "  (mistral-small-latest not found, skipping)"

echo
echo "==> Rule list for 'chat':"
"$BIN_DIR/proviz" --db-path "$DB" rule list --step chat

echo
echo "==> Dry-run CLI select (step=chat, tokens=2500):"
"$BIN_DIR/proviz" --db-path "$DB" select \
    --step chat --tokens 2500 --json-mode false --fn-call false \
    || echo "  (no models eligible — add rules first)"

echo
echo "==> Starting proviz-server in background..."
"$BIN_DIR/proviz-server" --db-path "$DB" --port 0 >"$PORT_FILE" 2>/dev/null &
SERVER_PID=$!

# Wait for PROVIZ_PORT=N line (up to 5s)
PORT=""
for i in $(seq 1 25); do
    PORT=$(grep -m1 "^PROVIZ_PORT=" "$PORT_FILE" 2>/dev/null | cut -d= -f2 || true)
    [ -n "$PORT" ] && break
    sleep 0.2
done

if [ -z "$PORT" ]; then
    echo "ERROR: server did not print PROVIZ_PORT within 5s" >&2
    exit 1
fi
echo "   Server listening on port $PORT"

BASE="http://localhost:$PORT"

echo
echo "==> GET /health:"
curl -sf "$BASE/health" | python3 -m json.tool 2>/dev/null || curl -s "$BASE/health"

echo
echo "==> POST /select (step=chat, tokens=2500):"
SELECT_RESP=$(curl -sf -X POST "$BASE/select" \
    -H "Content-Type: application/json" \
    -d '{"step":"chat","estimated_tokens":2500,"requires_fn_call":false,"requires_json_mode":false,"quality_min":0.0,"exclude_ids":[],"categories":[]}' \
    || echo '{"error":"no models — seed + add rules first"}')
echo "$SELECT_RESP" | python3 -m json.tool 2>/dev/null || echo "$SELECT_RESP"

MODEL_ID=$(echo "$SELECT_RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('model_id',''))" 2>/dev/null || true)

if [ -n "$MODEL_ID" ]; then
    echo
    echo "==> POST /report (success for model $MODEL_ID):"
    curl -sf -X POST "$BASE/report" \
        -H "Content-Type: application/json" \
        -d "{\"model_id\":\"$MODEL_ID\",\"outcome\":\"success\"}" \
        | python3 -m json.tool 2>/dev/null

    echo
    echo "==> POST /report (simulate TPM rate limit):"
    curl -sf -X POST "$BASE/report" \
        -H "Content-Type: application/json" \
        -d "{\"model_id\":\"$MODEL_ID\",\"outcome\":\"rate_limit\",\"error_type\":\"tpm\"}" \
        | python3 -m json.tool 2>/dev/null

    echo
    echo "==> POST /select again with model excluded (fallback test):"
    curl -sf -X POST "$BASE/select" \
        -H "Content-Type: application/json" \
        -d "{\"step\":\"chat\",\"estimated_tokens\":2500,\"exclude_ids\":[\"$MODEL_ID\"],\"requires_fn_call\":false,\"requires_json_mode\":false,\"quality_min\":0.0,\"categories\":[]}" \
        | python3 -m json.tool 2>/dev/null || echo "  (no fallback — only one model configured)"
fi

echo
echo "==> POST /catalog/reload:"
curl -sf -X POST "$BASE/catalog/reload" | python3 -m json.tool 2>/dev/null

echo
echo "==> Done."
