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
| `supports_reasoning_effort` | bool | Model accepts an OpenAI-style `reasoning_effort` param on `/complete` (e.g. "low"/"medium"/"high") |
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
| `canonical_key` | string? | Key into `pz_model_catalog` (below) identifying this model's underlying family across brands. When set and this row omits `quality_score`/`category`/`max_context_tokens`/capability flags, they're filled in from the matching catalog entry at load time — this row's own values, when present, always win. |
| `price_synced_at` | datetime? | When this row's pricing was last synced from a live provider catalog (e.g. OpenRouter). `NULL` for hand-curated providers — there's no "sync" to timestamp for those. |

## Model Catalog (`pz_model_catalog`)

Shared intrinsic properties for a model family, keyed by a manually-curated `canonical_key` (typically a HuggingFace `org/model` id). Lets brands that host the same underlying model share one `quality_score`/`category`/context/capability definition instead of re-curating it per brand — see [Providers](catalog-setup.md#shared-model-catalog-providersmodel_catalogjson). Purely additive: a `pz_models` row with no `canonical_key` is unaffected.

| Field | Type | Description |
|-------|------|-------------|
| `id` | UUID | Primary key |
| `canonical_key` | string | Unique family key, e.g. a HuggingFace `org/model` id |
| `display_name` | string? | Fallback display name |
| `category` | string? | Fallback category tag |
| `max_context_tokens` | int? | Fallback context window |
| `supports_function_calling` | bool? | Fallback capability flag |
| `supports_json_mode` | bool? | Fallback capability flag |
| `quality_score` | float? | Fallback 0.0–1.0 quality score |
| `knowledge_cutoff` | string? | Informational only, not used in selection |

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
