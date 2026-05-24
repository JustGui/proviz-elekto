# ProvizElekto

Smart LLM model router. Picks the best model for each call based on context size, rate limits, and capabilities - before the call happens.

```
Your app → ProvizElekto.select() → ModelCandidate → LiteLLM → Provider
                ↑                                        ↓
           report_success / report_rate_limit / report_error
```

**Key difference from LiteLLM fallback:** LiteLLM retries *after* failure. ProvizElekto picks the right model *before* the call - skipping models that are rate-limited, can't fit the context, or lack required capabilities.

## Features

- **Context-aware selection** - don't waste a 128k model on a 1k prompt
- **Rate-limit avoidance** - skips models hit by TPM/RPM limits (in-memory, O(1))
- **Capability filtering** - hard requirements for function calling, JSON mode
- **Quality floor** - reject models below a quality threshold per step
- **Your keys, your models** - curated catalog, no vendor proxy
- **Zero-infra** - `pip install proviz-elekto` auto-starts the Rust server as a subprocess
- **Any language** - HTTP API, not a library binding
- **Pluggable storage** - SQLite (default) or PostgreSQL

## Installation

```bash
pip install proviz-elekto
```

The `proviz-server` binary is bundled in the wheel.

CLI tool (`proviz`) is also included:

```bash
proviz --help
```

## Quickstart

```python
import os
from proviz_elekto import ProvizElekto

# Auto-starts proviz-server on first use. Stops on process exit.
pz = ProvizElekto(database_url=os.environ["DATABASE_URL"])
# or SQLite: pz = ProvizElekto(db_path="./proviz.db")

# Seed brands and models once
# proviz seed --brands --models

tried = []
while True:
    candidate = pz.select(
        step="verdict",
        estimated_tokens=2500,
        requires_json_mode=True,
        exclude_ids=tried,
    )
    try:
        import litellm, os
        result = litellm.completion(
            model=f"{candidate.brand_slug}/{candidate.model_slug}",
            messages=[{"role": "user", "content": "..."}],
            api_key=os.environ.get(candidate.api_key_env, ""),
        )
        pz.report_success(candidate.model_id)
        break
    except litellm.RateLimitError:
        pz.report_rate_limit(candidate.model_id, "tpm")
        tried.append(candidate.model_id)
    except Exception:
        pz.report_error(candidate.model_id, "other")
        tried.append(candidate.model_id)
```

## Catalog Setup

### 1. Seed built-in brands and models

```bash
# Against a running server (use the port printed by proviz-server on startup)
proviz seed --brands --models --server http://localhost:<PORT>

# Or directly against the database
proviz seed --brands --models --storage postgres --database-url $DATABASE_URL
proviz seed --brands --models --storage sqlite --db-path ./proviz.db
```

### 2. Add selection rules per step (optional)

Rules are optional. When no rules are defined for a step, ProvizElekto falls back to
all active models sorted by brand priority (see [Priority System](#priority-system)).

Rules give you fine-grained control: route small inputs to cheap models, require
function calling on a specific step, or cap context to avoid overkill.

```bash
# verdict step: cheap model for small inputs, quality model for large
proviz rule add --step verdict --model llama-3.1-8b-instant  --priority 1 --max-ctx 8000
proviz rule add --step verdict --model mistral-small-latest  --priority 2 --max-ctx 32000
proviz rule add --step verdict --model llama-3.3-70b-versatile --priority 3

# synthesis step: needs quality, context can be very large
proviz rule add --step synthesis --model mistral-small-latest    --priority 1 --max-ctx 30000
proviz rule add --step synthesis --model llama-3.3-70b-versatile --priority 2
proviz rule add --step synthesis --model mistral-large-2512      --priority 3

# planner step: cheap + fast
proviz rule add --step planner --model llama-3.1-8b-instant --priority 1

# agentic step: requires function calling
proviz rule add --step agentic --model mistral-small-latest  --priority 1 --fn-call
proviz rule add --step agentic --model mistral-large-2512    --priority 2 --fn-call

# detector step: fast, small context
proviz rule add --step detector --model llama-3.1-8b-instant --priority 1 --max-ctx 8000
proviz rule add --step detector --model mistral-small-latest --priority 2
```

### 3. Add a custom model

```bash
proviz model add \
  --brand mistral \
  --slug mistral-small-latest \
  --max-ctx 32000 \
  --price-in 0.10 --price-out 0.30 \
  --json-mode --function-calling \
  --quality 0.65 --latency-ms 400

# Or bulk import from JSON
proviz model import --file catalog.json
```

### 4. Dry-run a selection

```bash
proviz select --step verdict --tokens 2500 --json-mode
# selected:
#   brand:      groq
#   model:      llama-3.1-8b-instant
#   model_id:   b3f1...
#   api_key_env:GROQ_API_KEY
#   max_ctx:    128000
#   est_cost:   $0.000125
```

## Selection Algorithm

On every `select()` call (in-memory, ~microseconds):

1. Load step-specific rules from cache (sorted by `(brand.priority, rule.priority) ASC`).
   If no rules exist for the step, synthesize one rule per active model sorted by
   `brand.priority ASC` — no configuration required for generic steps.
2. Filter: `rule.is_enabled AND model.is_enabled AND brand.is_active`
3. Filter: `model.max_context_tokens >= estimated_tokens`
4. Filter: if `rule.max_ctx_tokens` set → `estimated_tokens <= rule.max_ctx_tokens` (avoid overkill)
5. Filter: capability requirements (function calling, JSON mode)
6. Filter: `quality_score >= quality_min` (skips models with unknown score when `quality_min > 0`)
7. Filter: `model_id NOT IN exclude_ids` (already tried this call)
8. Filter: not rate-limited (in-memory DashMap, O(1), TTL per error type)
9. Return first match or `409 AllModelsExhausted`

### Rate limit TTLs

| Error type | Cooldown |
|------------|----------|
| `tpm` (tokens/min) | 60s |
| `rpm` (requests/min) | 60s |
| `tpd` (tokens/day) | 3600s |
| `auth` | 300s |
| `timeout` | 30s |
| `parse` | 0s (logged, model not blocked) |
| `other` | 60s |

## Priority System

Two independent priority axes control selection order. Both use **lower = preferred**.

### Brand priority (`pz_brands.priority`)

Set when adding a brand. Determines which provider is tried first globally.

```bash
proviz brand add --slug mistral --name "Mistral AI" --priority 1
proviz brand add --slug groq    --name "Groq"        --priority 2
```

With priority 1, Mistral models are always tried before Groq models when both are eligible.

### Rule priority (`pz_selection_rules.priority`)

Set per rule. Within a step, rules are sorted by `(brand.priority, rule.priority)`.
Brand priority is the primary sort — two rules with the same rule priority but different
brands will still respect brand order.

```bash
# Rule priority 1 on brand.priority=2 loses to rule priority 99 on brand.priority=1
proviz rule add --step verdict --model llama-3.1-8b-instant  --priority 1  # groq (brand prio 2)
proviz rule add --step verdict --model mistral-small-latest  --priority 1  # mistral (brand prio 1) ← tried first
```

### Fallback order (no rules)

When a step has no rules, ProvizElekto falls back to all active models sorted by
`brand.priority`. Rule priority is irrelevant — only brand priority applies.
This means you can start using a new step name in your code without any catalog
changes as long as your brands are already configured.

## Quality Scores

`quality_score` is a float from `0.0` to `1.0` representing general text-reasoning
capability. It is used by callers to set a floor with `quality_min`:

```python
pz.select(step="verdict", estimated_tokens=2500, quality_min=0.7)
```

Models with a `NULL` score are excluded whenever `quality_min > 0`.

### Scoring rubric

| Range | Meaning | Examples |
|-------|---------|---------|
| 0.9 – 1.0 | Frontier-class: complex multi-step reasoning, high accuracy | Mistral Large, Llama 70B |
| 0.8 – 0.89 | Strong mid-tier: reliable for most tasks, good instruction following | Mistral Medium, Llama 8B instruct |
| 0.7 – 0.79 | Solid: works for structured tasks, weaker on open reasoning | Mistral Small, smaller instruct models |
| 0.6 – 0.69 | Minimal viable: classification, extraction, simple JSON | 3B–7B models |
| 0.0 | Not applicable | Embedding, audio, moderation, OCR, TTS |

Scores reflect public benchmarks (MMLU, MT-Bench) and community reputation.
Specialized models (audio, embedding, moderation) always score `0.0` — they are
excluded automatically when any `quality_min > 0` is requested.

### Built-in scores

The `providers/*/models.json` files in this repo are the source of truth for
built-in quality scores. They are loaded by `proviz providers` and `proviz seed`.
Scores in those files are reviewed periodically as new model versions are released.

To set or override a score on an existing model:

```bash
# Re-import after editing providers/groq/models.json
proviz providers --dir ./providers --storage postgres --database-url $DATABASE_URL

# Or set directly when adding a model
proviz model add --brand groq --slug llama-3.3-70b-versatile --max-ctx 131072 \
  --json-mode --function-calling --quality 0.85
```

## HTTP API

ProvizElekto exposes a local HTTP server. Any language can use it.

### Port handshake

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

### `POST /select`

```json
{
  "step": "verdict",
  "estimated_tokens": 2500,
  "requires_fn_call": false,
  "requires_json_mode": true,
  "quality_min": 0.6,
  "exclude_ids": []
}
```

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
  "estimated_input_cost_usd": 0.00148
}
```

Response `409` (all candidates exhausted):
```json
{ "error": "all_models_exhausted", "step": "verdict", "tried": 3 }
```

### `POST /report`

```json
{
  "model_id": "b3f1...",
  "outcome": "rate_limit",
  "error_type": "tpm"
}
```

`outcome`: `success` | `rate_limit` | `error`
`error_type`: `tpm` | `rpm` | `tpd` | `auth` | `timeout` | `parse` | `other`

### `GET /health`

```json
{ "status": "ok", "uptime_secs": 3600 }
```

### `POST /catalog/reload`

Hot-reload catalog from DB without restart.

```json
{ "status": "ok", "models_loaded": 12, "rules_loaded": 28 }
```

## Running the Server Manually

```bash
# SQLite (default, zero-infra) — port assigned by OS, printed to stdout
proviz-server --storage sqlite --db-path ./proviz.db

# Force a specific port
proviz-server --storage sqlite --db-path ./proviz.db --port 63130

# PostgreSQL (shares existing DB - tables are pz_* prefixed)
proviz-server --storage postgres --database-url "postgresql://user:pass@host/db"

# Via env vars
PROVIZ_STORAGE=postgres PROVIZ_DATABASE_URL=postgresql://... proviz-server
PROVIZ_PORT=63130 proviz-server  # force port
```

In all cases, the server prints `PROVIZ_PORT=<n>` to stdout immediately after binding.

## Data Model

### Brands (`pz_brands`)

| Field | Type | Description |
|-------|------|-------------|
| `id` | UUID | Primary key |
| `slug` | string | `groq`, `mistral`, `ollama` |
| `name` | string | Display name |
| `api_key_env` | string? | Env var holding the API key (`GROQ_API_KEY`) |
| `base_url` | string? | Optional API base URL override |
| `plan` | string? | Plan tier for this provider (e.g. `free`, `developer`). Models whose plan doesn't match are excluded from the cache. |
| `priority` | int16 | Selection order across brands — lower = tried first (default 0). Primary sort key in the [Priority System](#priority-system). |
| `is_active` | bool | Disable an entire provider without deleting |

### Models (`pz_models`)

| Field | Type | Description |
|-------|------|-------------|
| `id` | UUID | Primary key |
| `brand_id` | UUID | FK → `pz_brands` |
| `slug` | string | Actual API model name sent to provider |
| `max_context_tokens` | int | Hard context window limit |
| `max_output_tokens` | int? | Max output tokens |
| `supports_function_calling` | bool | Required for agentic steps |
| `supports_json_mode` | bool | Required for verdict/synthesis |
| `price_input_per_1m` | float? | USD per 1M input tokens |
| `price_output_per_1m` | float? | USD per 1M output tokens |
| `tpm_limit` | int? | Provider tokens/minute rate limit |
| `rpm_limit` | int? | Provider requests/minute rate limit |
| `rpd_limit` | int? | Provider requests/day rate limit |
| `tpd_limit` | int? | Provider tokens/day limit |
| `tpm_limit_month` | int? | Provider tokens/month limit |
| `rps_limit` | float? | Provider requests/second limit |
| `quality_score` | float? | 0.0–1.0 general text-reasoning capability. `NULL` models are excluded when `quality_min > 0`. See [Quality Scores](#quality-scores). |
| `avg_latency_ms` | int? | Known/estimated median latency |
| `is_enabled` | bool | Disable a model without deleting |

### Selection Rules (`pz_selection_rules`)

| Field | Type | Description |
|-------|------|-------------|
| `step` | string | Pipeline step name |
| `model_id` | UUID | FK → `pz_models` |
| `priority` | int16 | Secondary sort key within a step — lower = preferred. Brand priority takes precedence. |
| `max_ctx_tokens` | int? | Upper bound: skip this rule when `estimated_tokens > this` (avoids using a large-context model on a tiny input) |
| `requires_fn_call` | bool | Safety check (also filtered by model capability) |
| `is_enabled` | bool | Disable rule without deleting |

## Building from Source

```bash
git clone https://github.com/YOUR_ORG/proviz-elekto
cd proviz-elekto

# Build server + CLI
cargo build --release

# Run server
./target/release/proviz-server --storage sqlite --db-path ./dev.db

# Run CLI
./target/release/proviz --help

# Build Python wheel (requires maturin)
pip install maturin
cd python && maturin build --release
```

## Supported Providers (built-in seed)

| slug | Name |
|------|------|
| `groq` | Groq |
| `mistral` | Mistral AI |
| `ollama` | Ollama |

Add any provider supported by LiteLLM via `proviz brand add`.

## License

Apache-2.0
