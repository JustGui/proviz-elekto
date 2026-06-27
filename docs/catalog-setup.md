# Catalog Setup

## 1. Seed built-in brands and models

```bash
# Against a running server (use the port printed by proviz-server on startup)
proviz seed --brands --models --server http://localhost:<PORT>

# Or directly against the database
proviz seed --brands --models --storage postgres --database-url $DATABASE_URL
proviz seed --brands --models --storage sqlite --db-path ./proviz.db
```

```bash
# Update your catalog via Docker
curl -X POST http://localhost:63130/catalog/seed
```

## 2. Add selection rules per step (optional)

Rules are optional. When no rules are defined for a step, ProvizElekto falls back to
all active models sorted by brand priority (see [Priority System](selection-algorithm.md#priority-system)).

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

## 3. Add a custom model

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

## 4. Dry-run a selection

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
### Providers

Add any provider supported by LiteLLM via `proviz brand add`.
