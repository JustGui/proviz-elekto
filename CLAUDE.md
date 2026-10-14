# Contributor notes

Provider catalogue loading is in `crates/core/src/builtin_providers.rs`. The server's
`/complete` path is in `server/src/complete.rs`; Python `complete()` exposes the same
response contract. Format Rust changes with `cargo fmt` and test touched packages.

## Cloudflare Workers AI

`cloudflare_sync.rs` fetches account model search pages until empty (the reported
`total_count` can include inaccessible models), then the `format=openrouter`
capabilities. Only verified chat models enter ordinary chat selection. Non-chat
models are catalogue-only. Unknown prices/context remain unset. API failures,
invalid pages, repeated names, an empty chat catalogue or fewer than ten discovered
models preserve the last-good data. Sync uses the shared loader scoped to Cloudflare,
so unrelated providers' flags and limits are not rewritten. Missing models are disabled.

`providers/cloudflare/jev.json` supplies documented `typesafe/jev`, currently absent
from model search. Keep that supplement in the generated catalogue. Native decisions
reuse `server/src/systemone.rs`: only the Cloudflare URL and `input`/`result` envelopes
differ. Preserve choice validation, usage, confidence and `decision_probabilities`.
Do not route native Jev through chat completions or expose it to ordinary chat.

Credentials use `env_expand::provider_env`, shared with placeholder expansion;
CLOUDFLARE_API_KEY/ACCOUNT_ID take precedence over CLOUDFARE_API_KEY/ACCOUNT_ID.
Python split-flow chat mirrors the key fallback. Keep credentials and account URLs
out of fixtures, logs and commits. Cloudflare error bodies may echo customer input;
return only HTTP status on errors. Privacy citations live next to the sync mapper
and Jev supplement: no training for Cloudflare, ZDR for Jev only. Do not infer ZDR
for other models or alter other providers' flags.

Server: startup + PROVIZ_CLOUDFLARE_SYNC_SECS (3600 default). CLI:
`proviz providers sync-cloudflare [--dry-run]`. See README for deployment/group
provisioning. No storage migration or Python response field is needed.

Verification:
`cargo test -p proviz-elekto-core -p proviz-server -p proviz-elekto-storage-sqlite -p proviz`
`cargo clippy -p proviz-elekto-core -p proviz-server -p proviz-elekto-storage-sqlite -p proviz --all-targets`
`PYTHONPATH=python python -m unittest discover -s python/tests`
Use synthetic HTTP fixtures. Live smoke calls must use scratch SQLite and an
isolated local server, with credentials read at runtime and never printed.
