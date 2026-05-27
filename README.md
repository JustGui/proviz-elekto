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
- **Provider-anchored windows** - every successful call forwards `x-ratelimit-remaining-*` headers back to the server; the window floor is clamped to provider reality so internal estimates can't drift below what the provider actually sees
- **Scored selection** - picks the best model across all eligible candidates: headroom (50%), quality (25%), cost (15%), latency (10%)
- **Capability filtering** - hard requirements for function calling, JSON mode
- **Quality floor** - reject models below a quality threshold per step
- **Model groups** - define named pools of models (e.g. `"fast-chat"`, `"coding-tier1"`) and restrict selection to that pool
- **Your keys, your models** - curated catalog, no vendor proxy
- **Zero-infra** - `pip install proviz-elekto` auto-starts the Rust server as a subprocess
- **Any language** - HTTP API, not a library binding
- **Pluggable storage** - SQLite (default) or PostgreSQL

## Installation

ProvizElekto consists of a Rust server and various clients.

```bash
pip install proviz-elekto          # core only
pip install proviz-elekto[litellm] # + built-in LiteLLM integration
```

The `proviz-server` binary is bundled in the wheel.

CLI tool (`proviz`) is also included:

```bash
proviz --help
```

## Documentation

- [Catalog Setup](docs/catalog-setup.md) — Seeding brands/models, adding rules, model groups
- [Selection Algorithm](docs/selection-algorithm.md) — Scoring, headroom, priority, quality scores, retry hints
- [HTTP API Reference](docs/http-api.md) — `/select`, `/report`, `/health`, `/catalog/reload`
- [Deployment & Docker](docs/deployment.md) — Running the server, env vars, Docker, building from source
- [Data Model](docs/data-model.md) — Table schemas for all `pz_*` tables

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

    # Read provider rate-limit headers (Mistral/OpenAI style; Anthropic style also supported)
    hdrs = getattr(response, "_hidden_params", {}).get("additional_headers") or {}
    rem_req = hdrs.get("x-ratelimit-remaining-requests")
    rem_tok = hdrs.get("x-ratelimit-remaining-tokens")

    pz.report_success(
        candidate.model_id,
        estimated_tokens=candidate.estimated_tokens,  # releases in-flight reservation
        actual_tokens=response.usage.total_tokens,    # improves TPM window accuracy
        remaining_requests=int(rem_req) if rem_req is not None else None,
        remaining_tokens=int(rem_tok)   if rem_tok is not None else None,
    )
    # report_success is fire-and-forget — returns immediately, HTTP call runs in background
except RateLimitError as exc:
    msg = str(exc).lower()
    if "day" in msg or "daily" in msg:
        error_type = "tpd"
    elif "token" in msg:
        error_type = "tpm"
    else:
        error_type = "rpm"
    pz.report_rate_limit(candidate.model_id, error_type)  # synchronous — must complete before retry
except Exception:
    pz.report_error(candidate.model_id, "other")
```

`estimated_tokens` in each report call releases the in-flight reservation made at selection time. Omitting it is safe (legacy clients work unchanged) but leaves the in-flight counter inflated until the next selection clears it.

`report_success` is non-blocking: the HTTP call to proviz runs in a background daemon thread so the caller receives the LLM result without waiting for the round-trip. `report_rate_limit` and `report_error` remain synchronous because the model must be blocked in proviz before the retry `select()` call.

## License

Apache-2.0
