# HTTP API Reference

ProvizElekto exposes a local HTTP server. Any language can use it.

## Port handshake

The server binds to an OS-assigned ephemeral port by default (no port conflicts). After binding, it prints exactly one line to **stdout** before any other output:

```
PROVIZ_PORT=43912
```

All logs go to **stderr**. Clients must read this line to discover the port.

To force a specific port, set `PROVIZ_PORT=63130` (env) or pass `--port 63130` (CLI). The handshake line is still printed — clients always read it.

**Spawning from any language:**
```
start: proviz-server --port 0 [--storage ...] [--db-path ...]
read stdout line 1 → "PROVIZ_PORT=<n>"
parse port → use http://localhost:<n>/...
```

## `POST /select`

```json
{
  "step": "verdict",
  "estimated_tokens": 2500,
  "requires_fn_call": false,
  "requires_json_mode": true,
  "quality_min": 0.6,
  "exclude_ids": [],
  "group_name": "fast-chat"
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `step` | yes | Routing step name (used for error messages; rules are bypassed when group is set) |
| `estimated_tokens` | yes | Estimated input token count |
| `requires_fn_call` | no | Must support function calling |
| `requires_json_mode` | no | Must support JSON mode |
| `quality_min` | no | Minimum quality score (0.0 = no filter) |
| `exclude_ids` | no | Model UUIDs to skip (already-tried list) |
| `categories` | no | Restrict to specific model categories |
| `group_id` | no | Restrict candidates to this group (UUID). Takes priority over rules. |
| `group_name` | no | Restrict candidates to this group (slug). Takes priority over rules. |

Response `200`:
```json
{
  "model_id": "b3f1...",
  "brand_slug": "groq",
  "model_slug": "llama-3.3-70b-versatile",
  "api_key_env": "GROQ_API_KEY",
  "max_context_tokens": 128000,
  "supports_function_calling": true,
  "supports_json_mode": true,
  "estimated_input_cost_usd": 0.00148,
  "estimated_tokens": 2500
}
```

`estimated_tokens` echoes the value from the request. Echo it back in `/report` so the
server can release the in-flight reservation and keep quota windows accurate.

Response `409` (all candidates exhausted):
```json
{ "error": "all_models_exhausted", "step": "verdict", "tried": 3 }
```

## `POST /report`

```json
{
  "model_id": "b3f1...",
  "outcome": "success",
  "estimated_tokens": 2500,
  "actual_tokens": 1843,
  "remaining_requests": 47,
  "remaining_tokens": 82340
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `model_id` | yes | UUID from `/select` response |
| `outcome` | yes | `success` \| `rate_limit` \| `error` |
| `error_type` | for `rate_limit`/`error` | `tpm` \| `rpm` \| `tpd` \| `auth` \| `timeout` \| `parse` \| `other` |
| `estimated_tokens` | recommended | Echo of `ModelCandidate.estimated_tokens` — releases the in-flight reservation |
| `actual_tokens` | optional | Real token count from provider — improves TPM window accuracy |
| `remaining_requests` | optional | Value of `x-ratelimit-remaining-requests` (or `anthropic-ratelimit-requests-remaining`) from the provider response. Anchors the RPM window floor to provider reality. |
| `remaining_tokens` | optional | Value of `x-ratelimit-remaining-tokens` (or `anthropic-ratelimit-tokens-remaining`) from the provider response. Anchors the TPM window floor to provider reality. |

All fields except `model_id` and `outcome` are optional for backward compatibility. `remaining_requests`/`remaining_tokens` should be sent on every `success` outcome when the provider includes rate-limit headers — they prevent internal window estimates from drifting below what the provider actually sees, reducing unnecessary over-booking.

## `GET /health`

```json
{ "status": "ok", "uptime_secs": 3600 }
```

## `POST /catalog/reload`

Hot-reload catalog from DB without restart.

```json
{ "status": "ok", "models_loaded": 12, "rules_loaded": 28 }
```
