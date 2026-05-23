#!/usr/bin/env python3
"""
proviz-elekto Python client demo.

Prerequisites:
  1. cargo build --release --bin proviz --bin proviz-server
  2. Seed the catalog:
       ./target/release/proviz --db-path ./demo.db seed --brands --models
  3. Add at least one selection rule, e.g.:
       ./target/release/proviz --db-path ./demo.db rule add \
           --step chat --model llama-3.3-70b-versatile --priority 0

Then run:
  python3 examples/python/basic_usage.py
"""

import os
import sys

# Allow running from repo root without installing the package
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "../../python"))

from proviz_elekto import AllModelsExhausted, ProvizElekto

DB = os.environ.get("PROVIZ_DB", "./demo.db")

print(f"Connecting to proviz-server (db={DB})...")
pz = ProvizElekto(db_path=DB)

print(f"Health: {pz.health()}")
print()

# ── Basic select ──────────────────────────────────────────────────────────────
print("==> Select for step 'chat' (2500 tokens, json_mode required):")
try:
    candidate = pz.select(
        step="chat",
        estimated_tokens=2500,
        requires_json_mode=True,
        quality_min=0.0,
    )
    print(f"  Selected: {candidate['brand_slug']}/{candidate['model_slug']}")
    print(f"  model_id: {candidate['model_id']}")
    print(f"  max_ctx:  {candidate['max_context_tokens']:,} tokens")
    print(f"  fn_call:  {candidate['supports_function_calling']}")
    if candidate.get("estimated_input_cost_usd") is not None:
        print(f"  est_cost: ${candidate['estimated_input_cost_usd']:.6f}")

    model_id = candidate["model_id"]

    # Report success after a real LLM call would go here
    pz.report_success(model_id)
    print("  Reported: success")

except AllModelsExhausted as e:
    print(f"  No model available: {e}")
    print("  -> Make sure catalog is seeded and rules added (see prerequisites above).")
    sys.exit(1)

print()

# ── Simulate rate limit + fallback ───────────────────────────────────────────
print("==> Simulating TPM rate limit on that model...")
pz.report_rate_limit(model_id, error_type="tpm")
print(f"  Rate-limited: {model_id}")

print()
print("==> Select again with that model excluded (fallback):")
try:
    fallback = pz.select(
        step="chat",
        estimated_tokens=2500,
        exclude_ids=[model_id],
    )
    print(f"  Fallback: {fallback['brand_slug']}/{fallback['model_slug']}")
    pz.report_success(fallback["model_id"])
except AllModelsExhausted:
    print("  No fallback available (only one model configured — that's fine for demo).")

print()

# ── Catalog reload ────────────────────────────────────────────────────────────
print("==> Reloading catalog:")
result = pz.reload()
print(f"  {result}")

print()
print("Done.")
