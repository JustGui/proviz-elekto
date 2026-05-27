# Data Model

All tables use a `pz_` prefix to coexist with existing databases. Schema is auto-created on first run.

## Brands (`pz_brands`)

| Field | Type | Description |
|-------|------|-------------|
| `id` | UUID | Primary key |
| `slug` | string | `groq`, `mistral`, `ollama` |
| `name` | string | Display name |
| `api_key_env` | string? | Env var holding the API key (`GROQ_API_KEY`) |
| `base_url` | string? | Optional API base URL override |
| `plan` | string? | Plan tier for this provider (e.g. `free`, `developer`). Models whose plan doesn't match are excluded from the cache. |
| `priority` | int16 | Selection order across brands — lower = tried first (default 0). Primary sort key in the [Priority System](selection-algorithm.md#priority-system). |
| `is_active` | bool | Disable an entire provider without deleting |

## Models (`pz_models`)

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
| `quality_score` | float? | 0.0–1.0 general text-reasoning capability. `NULL` models are excluded when `quality_min > 0`. See [Quality Scores](selection-algorithm.md#quality-scores). |
| `avg_latency_ms` | int? | Known/estimated median latency |
| `is_enabled` | bool | Disable a model without deleting |

## Selection Rules (`pz_selection_rules`)

| Field | Type | Description |
|-------|------|-------------|
| `step` | string | Pipeline step name |
| `model_id` | UUID | FK → `pz_models` |
| `priority` | int16 | Secondary sort key within a step — lower = preferred. Brand priority takes precedence. |
| `max_ctx_tokens` | int? | Upper bound: skip this rule when `estimated_tokens > this` (avoids using a large-context model on a tiny input) |
| `requires_fn_call` | bool | Safety check (also filtered by model capability) |
| `is_enabled` | bool | Disable rule without deleting |

## Groups (`pz_groups`)

| Field | Type | Description |
|-------|------|-------------|
| `id` | UUID | Primary key |
| `slug` | string | Human-readable key, e.g. `fast-chat` |
| `name` | string | Display name |
| `description` | string? | Optional description |
| `is_active` | bool | Disabled groups return `group_not_found` on select |

## Group Members (`pz_group_members`)

| Field | Type | Description |
|-------|------|-------------|
| `group_id` | UUID | FK → `pz_groups` (cascades on delete) |
| `model_id` | UUID | FK → `pz_models` (cascades on delete) |
| `priority` | int16 | Tiebreaker within group — lower = preferred (alongside brand priority) |
| `is_enabled` | bool | Disable a member without removing it |
