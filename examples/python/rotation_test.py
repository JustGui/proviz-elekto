#!/usr/bin/env python3
"""
rotation_test.py — demonstrate proviz-elekto model rotation under load.

What it does:
  1. Generates prompts at several token sizes (tiny → large) using tiktoken.
  2. Fires them all quickly via pz.select(), printing which model was chosen.
  3. Runs a burst phase where each selected model gets rate-limited immediately,
     forcing the selector to rotate through every available candidate.

Prerequisites:
  1. cargo build --release --bin proviz --bin proviz-server
  2. Seed the catalog:
       ./target/release/proviz --db-path ./demo.db seed --brands --models
  3. Add selection rules for step "chat", e.g.:
       ./target/release/proviz --db-path ./demo.db rule add \
           --step chat --model llama-3.3-70b-versatile --priority 0
       ./target/release/proviz --db-path ./demo.db rule add \
           --step chat --model gemini-1.5-flash --priority 1
       ./target/release/proviz --db-path ./demo.db rule add \
           --step chat --model mistral-small --priority 2

  pip install tiktoken   # only dep beyond proviz-elekto itself

Run:
  python3 examples/python/rotation_test.py
"""

import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "../../python"))

try:
    import tiktoken
except ImportError:
    sys.exit("tiktoken not installed — run: pip install tiktoken")

from proviz_elekto import AllModelsExhausted, ProvizElekto

# ── helpers ───────────────────────────────────────────────────────────────────

ENC = tiktoken.get_encoding("cl100k_base")


def make_text(target_tokens: int) -> str:
    """Return a string that encodes to approximately target_tokens tokens."""
    word = "lorem "
    word_tokens = len(ENC.encode(word))
    repeats = max(1, target_tokens // word_tokens)
    text = word * repeats
    # Trim or pad to hit the target exactly
    tokens = ENC.encode(text)
    if len(tokens) > target_tokens:
        text = ENC.decode(tokens[:target_tokens])
    return text


def count_tokens(text: str) -> int:
    return len(ENC.encode(text))


def select_and_print(pz: ProvizElekto, label: str, tokens: int, report_ok: bool = True) -> str | None:
    """Select a model, print the result, optionally report success. Returns model_id or None."""
    try:
        c = pz.select(step="chat", estimated_tokens=tokens)
        print(f"  [{label:>12}]  {tokens:>6} tokens  →  {c.brand_slug}/{c.model_slug}")
        if report_ok:
            pz.report_success(c.model_id)
        return c.model_id
    except AllModelsExhausted:
        print(f"  [{label:>12}]  {tokens:>6} tokens  →  (all models exhausted)")
        return None


# ── main ──────────────────────────────────────────────────────────────────────

DB = os.environ.get("PROVIZ_DB", "./demo.db")

print(f"Connecting to proviz-server (db={DB})...")
pz = ProvizElekto(db_path=DB)
print(f"Health: {pz.health()}\n")

# ── Phase 1: different token sizes ────────────────────────────────────────────

SIZES = [100, 500, 2_000, 8_000, 32_000]

print("=" * 60)
print("Phase 1 — one request per token level")
print("=" * 60)

for target in SIZES:
    text = make_text(target)
    actual = count_tokens(text)
    select_and_print(pz, f"{actual} tok", actual)

print()

# ── Phase 2: rapid burst ──────────────────────────────────────────────────────

BURST = 20
BURST_TOKENS = 1_000

print("=" * 60)
print(f"Phase 2 — rapid burst ({BURST} requests, {BURST_TOKENS} tokens each)")
print("=" * 60)

seen: dict[str, int] = {}
t0 = time.monotonic()

for i in range(BURST):
    try:
        c = pz.select(step="chat", estimated_tokens=BURST_TOKENS)
        key = f"{c.brand_slug}/{c.model_slug}"
        seen[key] = seen.get(key, 0) + 1
        print(f"  [{i+1:>2}/{BURST}]  →  {key}")
        pz.report_success(c.model_id)
    except AllModelsExhausted:
        print(f"  [{i+1:>2}/{BURST}]  →  (all models exhausted)")
        break

elapsed = time.monotonic() - t0
print(f"\n  Burst finished in {elapsed:.2f}s")
print(f"  Distribution: {dict(sorted(seen.items(), key=lambda x: -x[1]))}")

print()

# ── Phase 3: forced rotation via rate-limit reports ──────────────────────────

ROTATION_ROUNDS = 15

print("=" * 60)
print(f"Phase 3 — forced rotation (report tpm after each selection)")
print("=" * 60)

seen_rotation: dict[str, int] = {}

for i in range(ROTATION_ROUNDS):
    try:
        c = pz.select(step="chat", estimated_tokens=BURST_TOKENS)
        key = f"{c.brand_slug}/{c.model_slug}"
        seen_rotation[key] = seen_rotation.get(key, 0) + 1
        # Immediately mark as rate-limited to force the next call onto another model
        pz.report_rate_limit(c.model_id, error_type="tpm")
        print(f"  [{i+1:>2}/{ROTATION_ROUNDS}]  →  {key}  (tpm rate-limited)")
    except AllModelsExhausted:
        print(f"  [{i+1:>2}/{ROTATION_ROUNDS}]  →  (all models exhausted — pool drained)")
        break

print(f"\n  Rotation distribution: {dict(sorted(seen_rotation.items(), key=lambda x: -x[1]))}")
unique = len(seen_rotation)
print(f"  Unique models exercised: {unique}")

print()
print("Done.")
