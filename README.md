# ProvizElekto

Smart LLM model router. Picks the best model for each call based on context size, rate limits, and capabilities — and retries automatically on failure.

```
Your app → pz.call(step, fn)               → CallResult
           pz.call_litellm(step, messages) → CallResult
                    ↕  (automatic)
           select → LLM call → report → retry on failure
                    ↕
              proviz-server (Rust)
          rate-limit state · catalog
```

**Key difference from LiteLLM fallback:** LiteLLM retries *after* failure. ProvizElekto picks the right model *before* the call — skipping models that are rate-limited or near their quota, can't fit the context, or lack required capabilities — then retries with the next eligible model automatically.

## Features

- **Context-aware selection** - don't waste a 128k model on a 1k prompt
- **Proactive quota tracking** - sliding-window counters (RPM/TPM/RPD/TPD) plus atomic in-flight reservations; avoids over-booking before any 429 fires
- **Scored selection** - picks the best model across all eligible candidates: headroom (50%), quality (25%), cost (15%), latency (10%)
- **Capability filtering** - hard requirements for function calling, JSON mode
- **Quality floor** - reject models below a quality threshold per step
- **Model groups** - define named pools of models (e.g. `"fast-chat"`, `"coding-tier1"`) and restrict selection to that pool
- **Your keys, your models** - curated catalog, no vendor proxy
- **Zero-infra** - `pip install proviz-elekto` auto-starts the Rust server as a subprocess
- **Any language** - HTTP API, not a library binding
- **Pluggable storage** - SQLite (default) or PostgreSQL

## Installation

```bash
pip install proviz-elekto          # core only
pip install proviz-elekto[litellm] # + built-in LiteLLM integration
```

The `proviz-server` binary is bundled in the wheel.

CLI tool (`proviz`) is also included:

```bash
proviz --help
```

## Quickstart

### With LiteLLM (recommended)

```python
from proviz_elekto import ProvizElekto

pz = ProvizElekto(db_path="./proviz.db")
# or PostgreSQL: pz = ProvizElekto(database_url=os.environ["DATABASE_URL"])

result = pz.call_litellm(
    step="verdict",
    messages=[{"role": "user", "content": "Summarize this document..."}],
    estimated_tokens=2500,
    requires_json_mode=True,
)
print(result.provider, result.candidate.model_slug, result.total_tokens)
# → mistral mistral-small-latest 312
```

`call_litellm()` selects the best available model, calls it, reports the outcome, and retries with the next eligible model on any failure — automatically.

### With a custom LLM caller

```python
import anthropic

client = anthropic.Anthropic()

def my_llm(candidate):
    return client.messages.create(
        model=candidate.model_slug,
        max_tokens=1024,
        messages=[{"role": "user", "content": "Hello"}],
    )

result = pz.call("verdict", my_llm, estimated_tokens=100)
print(result.candidate.brand_slug, result.prompt_tokens)
```

Pass any callable that accepts a `ModelCandidate` and returns a response. ProvizElekto wraps it with the same select → report → retry loop.

### Low-level API

If you need direct control over selection and reporting:

```python
candidate = pz.select(step="verdict", estimated_tokens=2500)
try:
    response = my_llm_call(candidate)
    pz.report_success(
        candidate.model_id,
        estimated_tokens=candidate.estimated_tokens,  # echo back for accurate quota tracking
        actual_tokens=response.usage.total_tokens,    # improves TPM window accuracy
    )
except RateLimitError:
    pz.report_rate_limit(
        candidate.model_id, "tpm",
        estimated_tokens=candidate.estimated_tokens,
    )
except Exception:
    pz.report_error(
        candidate.model_id, "other",
        estimated_tokens=candidate.estimated_tokens,
    )
```

`estimated_tokens` in each report call releases the in-flight reservation made at selection time. Omitting it is safe (legacy clients work unchanged) but leaves the in-flight counter inflated until the next selection clears it.

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

## Model Groups

Groups let you define named pools of models and select from them by name instead of configuring per-step rules. The group restricts the candidate pool; the normal brand-priority + scoring algorithm picks the winner within it.

### Create and populate a group

```bash
proviz group add --slug fast-chat --name "Fast Chat Models"
proviz group member add --group fast-chat --model llama-3.1-8b-instant --priority 0
proviz group member add --group fast-chat --model mistral-small-latest  --priority 1

# List groups
proviz group list

# List members of a group
proviz group member list --group fast-chat

# Remove a model from a group
proviz group member remove --group fast-chat --model mistral-small-latest

# Disable / enable a group
proviz group disable --slug fast-chat
proviz group enable  --slug fast-chat

# Delete a group (also deletes all members)
proviz group delete --slug fast-chat
```

### Select from a group

```bash
# By slug
proviz select --step default --tokens 1000 --group-name fast-chat

# By UUID
proviz select --step default --tokens 1000 --group-id <uuid>
```

Python:
```python
candidate = pz.select("default", 1000, group_name="fast-chat")
# or
candidate = pz.select("default", 1000, group_id="<uuid>")
```

HTTP:
```json
POST /select
{
  "step": "default",
  "estimated_tokens": 1000,
  "group_name": "fast-chat"
}
```

### How groups interact with rules

When `group_id` or `group_name` is provided:
- The group defines the **candidate pool** — only models in the group are eligible
- Step rules are **bypassed entirely**
- The normal scoring formula (headroom + quality + cost + latency) still applies within the pool
- All other filters (capabilities, rate limits, context size, quality floor) still apply

Response `404` when group slug/UUID is unknown or the group is inactive:
```json
{ "error": "group_not_found", "group": "fast-chat" }
```

## Selection Algorithm

On every `select()` call (in-memory, ~microseconds):

### Pass 1 — Hard filters (eliminate ineligible candidates)

1. Load step-specific rules from cache (sorted by `(brand.priority, rule.priority) ASC`).
   If no rules exist for the step, synthesize one rule per active model sorted by
   `brand.priority ASC` — no configuration required for generic steps.
2. Filter: `rule.is_enabled AND model.is_enabled AND brand.is_active`
3. Filter: `model.max_context_tokens >= estimated_tokens`
4. Filter: if `rule.max_ctx_tokens` set → `estimated_tokens <= rule.max_ctx_tokens` (avoid overkill)
5. Filter: capability requirements (function calling, JSON mode)
6. Filter: `quality_score >= quality_min` (skips models with unknown score when `quality_min > 0`)
7. Filter: `model_id NOT IN exclude_ids` (already tried this call)
8. Filter: not blocked by reactive rate-limit state (in-memory DashMap, O(1), TTL per error type)
9. Filter: proactive headroom check — `headroom(model) >= 0` where headroom uses sliding-window
   counters (RPM/TPM/RPD/TPD) plus atomic in-flight reservations. Negative headroom = over quota.

### Pass 2 — Score and pick best

All candidates that pass the filters are scored:

```
score = 0.50 × headroom       (0 = last slot, 1 = fully unconstrained)
      + 0.25 × quality_score   (model.quality_score, default 0.5 if unknown)
      + 0.15 × cost_score      (min-max normalized across candidates; cheaper = higher)
      + 0.10 × latency_score   (min-max normalized across candidates; faster = higher)
```

The highest-scoring candidate wins. When scores tie, **rule priority breaks the tie** (lower
number = preferred), preserving your explicit ordering for equal-quality models.

Before returning, the winner's in-flight slot is atomically reserved so concurrent
`select()` calls spread load across models rather than all grabbing the same one.

### Rate-limit TTLs (reactive blocking)

Reactive blocking (from `/report rate_limit`) coexists with the proactive headroom system.
A model can be reactive-blocked even when its quota counters show headroom.

| Error type | Cooldown |
|------------|----------|
| `tpm` (tokens/min) | 60s |
| `rpm` (requests/min) | 60s |
| `tpd` (tokens/day) | 3600s |
| `auth` | 300s |
| `timeout` | 30s |
| `parse` | 0s (logged, model not blocked) |
| `other` | 60s |

### Quota sliding windows

Proactive quota tracking uses four per-model sliding windows:

| Dimension | Window | Limit field |
|-----------|--------|-------------|
| RPM | 60 s | `rpm_limit` |
| TPM | 60 s | `tpm_limit` |
| RPD | 24 h | `rpd_limit` |
| TPD | 24 h | `tpd_limit` |

Windows are updated on every `/report` call. In-flight reservations
(made at selection time, released on report) are counted on top of the
window sums when computing headroom — preventing over-booking under
concurrent load.

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

### `POST /report`

```json
{
  "model_id": "b3f1...",
  "outcome": "rate_limit",
  "error_type": "tpm",
  "estimated_tokens": 2500,
  "actual_tokens": 1843
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `model_id` | yes | UUID from `/select` response |
| `outcome` | yes | `success` \| `rate_limit` \| `error` |
| `error_type` | for `rate_limit`/`error` | `tpm` \| `rpm` \| `tpd` \| `auth` \| `timeout` \| `parse` \| `other` |
| `estimated_tokens` | recommended | Echo of `ModelCandidate.estimated_tokens` — releases the in-flight reservation |
| `actual_tokens` | optional | Real token count from provider — improves TPM window accuracy |

`estimated_tokens` and `actual_tokens` are optional for backward compatibility. Omitting
`estimated_tokens` leaves the in-flight counter inflated, which is safe (pessimistic) but
causes the model to appear more loaded than it is until the in-flight window clears.

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

## Docker

### Pull from Docker Hub

```bash
docker pull justgu1/proviz-elekto:latest
```

### Run with PostgreSQL

```bash
docker run -d \
  --name proviz \
  -p 63130:63130 \
  -e PROVIZ_STORAGE=postgres \
  -e PROVIZ_DATABASE_URL="postgresql://user:pass@host/db" \
  justgu1/proviz-elekto:latest
```

### Run with a named Docker volume (PostgreSQL recommended for production)

```bash
docker run -d \
  --name proviz \
  -p 63130:63130 \
  -e PROVIZ_STORAGE=postgres \
  -e PROVIZ_DATABASE_URL="postgresql://user:pass@host/db" \
  justgu1/proviz-elekto:latest
```

### Docker Compose (recommended)

```yaml
services:
  proviz:
    image: justgu1/proviz-elekto:latest
    ports:
      - "63130:63130"
    environment:
      PROVIZ_STORAGE: postgres
      PROVIZ_DATABASE_URL: postgresql://user:pass@db/mydb
      PROVIZ_PORT: 63130
    depends_on:
      - db

  db:
    image: postgres:16-alpine
    environment:
      POSTGRES_USER: user
      POSTGRES_PASSWORD: pass
      POSTGRES_DB: mydb
    volumes:
      - pgdata:/var/lib/postgresql/data

volumes:
  pgdata:
```

### Python client with Docker

Point the Python client at the running container using env vars or constructor args:

```python
import os
from proviz_elekto import ProvizElekto

# Via env vars (no code change needed)
# PROVIZ_HOST=proviz PROVIZ_PORT=63130

# Or via constructor
pz = ProvizElekto(host="proviz", port=63130)
```

`PROVIZ_HOST` and `PROVIZ_PORT` env vars are read automatically; a non-localhost host with a
non-zero port skips subprocess spawning and attaches directly to the running container.

### Build the image locally

```bash
docker build -t proviz-elekto .
docker run -p 63130:63130 -e PROVIZ_DATABASE_URL=postgresql://... proviz-elekto
```

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

### Groups (`pz_groups`)

| Field | Type | Description |
|-------|------|-------------|
| `id` | UUID | Primary key |
| `slug` | string | Human-readable key, e.g. `fast-chat` |
| `name` | string | Display name |
| `description` | string? | Optional description |
| `is_active` | bool | Disabled groups return `group_not_found` on select |

### Group Members (`pz_group_members`)

| Field | Type | Description |
|-------|------|-------------|
| `group_id` | UUID | FK → `pz_groups` (cascades on delete) |
| `model_id` | UUID | FK → `pz_models` (cascades on delete) |
| `priority` | int16 | Tiebreaker within group — lower = preferred (alongside brand priority) |
| `is_enabled` | bool | Disable a member without removing it |

## Building from Source

```bash
git clone https://github.com/JustGui/proviz-elekto
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
