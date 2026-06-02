# Selection Algorithm

On every `select()` call (in-memory, ~microseconds):

## Pass 1 — Hard filters (eliminate ineligible candidates)

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

**Headroom is not a hard filter.** A model that is over its per-minute quota (negative fast
headroom) stays in the candidate pool with a lower score. This guarantees `AllModelsExhausted`
is never returned while any model is still reachable — the system will always route to the
least-over-quota option. `AllModelsExhausted` is only raised when every candidate is blocked
by the reactive rate-limit state (i.e., a provider returned a 429 and is in cooldown).

## Pass 2 — Score and pick best

Headroom is split into two components to give different weight to fast vs slow recovery:

- **`fast_headroom`** (RPS/RPM/TPM windows, ≤60 s): recovers quickly — penalised lightly
- **`slow_headroom`** (RPD/TPD windows, 24 h): irreplaceable daily budget — penalised heavily

Both values are mapped `[-1, 1] → [0, 1]` before scoring: `(headroom.clamp(-1,1) + 1) / 2`.

### Without group (step-based selection)

```
score = 0.25 × fast_hr_norm    (RPS/RPM/TPM, ≤60s recovery)
      + 0.20 × slow_hr_norm    (RPD/TPD, 24h recovery — preserve daily budget)
      + 0.20 × quality_score   (model.quality_score, default 0.5 if unknown)
      + 0.15 × cost_score      (min-max normalized across candidates; cheaper = higher)
      + 0.10 × latency_score   (min-max normalized across candidates; faster = higher)
      + 0.10 × traffic_balance (steers load toward under-served brands; see below)
```

### With group (`use_member_priority=true`)

```
score = 0.20 × fast_hr_norm
      + 0.15 × slow_hr_norm
      + 0.20 × quality_score
      + 0.15 × cost_score
      + 0.10 × latency_score
      + 0.10 × priority_score  (lower member.priority → higher score; 1.0 when all equal)
      + 0.10 × traffic_balance
```

The highest-scoring candidate wins. When scores tie, **rule priority breaks the tie** (lower
number = preferred), preserving your explicit ordering for equal-quality models.

Before returning, the winner's in-flight slot is atomically reserved so concurrent
`select()` calls spread load across models rather than all grabbing the same one.

## Traffic balance (`traffic_weight`)

Each brand has a `traffic_weight` (default `1.0`, stored in `pz_brands.traffic_weight`).
The selector tracks per-brand selection counts in a 5-minute rolling window and computes
how much of that window each brand consumed vs its entitlement:

```
target_share  = brand.traffic_weight / sum(all_pool_brands.traffic_weight)
actual_share  = brand_recent_count / total_recent_count
balance_ratio = target_share / (actual_share + ε)   # high when under-served
```

The ratio is min-max normalised across the candidate pool before contributing to the score.
When all brands are equally served (or only one brand is in the pool), the traffic component
has no effect. When one brand has received less traffic than its weight entitles it to,
its models get a higher score on this component.

```bash
# Give Groq twice the traffic share of Mistral
proviz brand set-traffic-weight --slug groq    --weight 2.0
proviz brand set-traffic-weight --slug mistral --weight 1.0

# Or at creation time
proviz brand add --slug groq --name "Groq" --traffic-weight 2.0
```

## Rate-limit TTLs (reactive blocking)

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

## Quota sliding windows

Proactive quota tracking uses five sliding windows, maintained per `(model, API key)` bucket:

| Dimension | Window | Limit field |
|-----------|--------|-------------|
| RPS | 1 s | `rps_limit` |
| RPM | 60 s | `rpm_limit` |
| TPM | 60 s | `tpm_limit` |
| RPD | 24 h | `rpd_limit` |
| TPD | 24 h | `tpd_limit` |

Windows are updated on every `/report` call. In-flight reservations
(made at selection time, released on report) are counted on top of the
window sums when computing headroom — preventing over-booking under
concurrent load.

When `remaining_requests` or `remaining_tokens` are included in a `/report` payload,
the server stores them as a floor for the corresponding window: `effective_used = max(window_sum, limit - remaining)`.
This means if the provider reports fewer remaining requests/tokens than the local window
suggests, the server trusts the provider. In-flight requests (not yet acknowledged by
the provider) are still added on top.

### Per-key isolation (multi-account brands)

When a brand has multiple API keys in `pz_brand_api_keys` (separate accounts), each window
set above is tracked independently per key — the tracker is keyed by `(model_id, brand_key_id)`.
The selector picks the serving key during Pass 1, then computes headroom against *that key's*
bucket. So if you register two Mistral accounts under one brand:

- Heavy usage on key A drains key A's windows only; key B keeps full headroom and is scored
  higher on the next selection, naturally spreading load across both accounts' quotas.
- Provider `x-ratelimit-remaining-*` headers (which are per-account) anchor the floor of the
  key that produced them, instead of overwriting a single shared value.
- Provider `x-ratelimit-limit-*` headers anchor the *ceiling* per key (when `sync_limits=true`),
  and headroom prefers it over the DB `rpm_limit`/`tpm_limit` (`provider_limit.or(db_limit)`).
  Two accounts with different real limits are each scored against their own. The model-level DB
  row is only written for single-key brands — a shared write would clobber across accounts.
- The caller echoes `brand_key_id` back in `/report`, so the in-flight release lands on the
  same bucket the reservation was taken on.

Single-key/legacy brands use `brand_key_id = None` — one bucket per model, exactly as before.

## Transient exhaustion and retry hints

When all models fail Pass 1 (reactive-blocked), `AllModelsExhausted` is raised (HTTP 409).
The response includes a `retry_after_ms` hint computed from the remaining reactive-block TTLs:

```json
{
  "error": "all_models_exhausted",
  "step": "detector",
  "tried": 14,
  "retry_after_ms": 1820
}
```

**Python `call()` / `call_litellm()` built-in retry:**

```python
# Retry for up to 60 s when transiently exhausted (default when using model_selector.py)
result = pz.call_litellm(
    step="detector",
    messages=messages,
    max_wait_secs=60,   # sleep retry_after_ms, keep retrying until budget spent
)

# Disable retry (raise immediately)
result = pz.call_litellm(step="detector", messages=messages, max_wait_secs=0)
```

The sleep uses `retry_after_ms × uniform(0.8, 1.2)` jitter so concurrent workers don't
all wake up and hammer the same freed slots simultaneously.

When using `model_selector.py` (the rtfc wrapper), the default is controlled by the
`PROVIZ_MAX_WAIT_SECS` environment variable (default `60`).

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
