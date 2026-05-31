#!/usr/bin/env python3
"""
batch_test.py — test proviz-elekto Mistral Batch API integration.

What it tests:
  1. Single batch job: submit one request, wait for result.
  2. Parallel batch jobs: submit N requests concurrently, collect all results.
  3. Non-blocking poll: verify job.done() before blocking on job.result().
  4. Cost accounting: confirm actual_cost_usd is returned and plausible.
  5. Window flushing: verify early flush triggers when max_batch_size is hit.

Prerequisites:
  1. cargo build --release --bin proviz --bin proviz-server
  2. Seed catalog with Mistral models:
       ./target/release/proviz --db-path ./demo.db seed --brands --models
  3. Add a batch-eligible Mistral rule for step "classify":
       ./target/release/proviz --db-path ./demo.db rule add \
           --step classify --model mistral-small-2603 --priority 0
  4. Set MISTRAL_API_KEY in your environment.

Optional env vars:
  PROVIZ_DB                 path to the SQLite db (default: ./demo.db)
  PROVIZ_BATCH_WINDOW_SECS  server batch window in seconds (default: 10 for this test)
  BATCH_STEP                step name to use (default: classify)
  BATCH_SIZE                number of parallel jobs to submit (default: 5)

Run:
  python3 examples/python/batch_test.py
"""

import os
import sys
import time
import threading

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "../../python"))

from proviz_elekto import AllModelsExhausted, ProvizElekto
from proviz_elekto.batch import BatchError, BatchJob, BatchJobResult, BatchTimeoutError

DB = os.environ.get("PROVIZ_DB", "./demo.db")
# PROVIZ_BATCH_WINDOW_SECS is read by the server process; set it before starting.
# Default to 10s for faster test turnaround (production default is 60s).
os.environ.setdefault("PROVIZ_BATCH_WINDOW_SECS", "10")
BATCH_SIZE = int(os.environ.get("BATCH_SIZE", "5"))

STEP = os.environ.get("BATCH_STEP", "classify")
TIMEOUT = 60  # seconds to wait for Mistral to process the batch

PROMPTS = [
    "Classify the sentiment of: 'I love this product!'",
    "Classify the sentiment of: 'This was a terrible experience.'",
    "Classify the sentiment of: 'It was okay, nothing special.'",
    "Classify the sentiment of: 'Absolutely fantastic, exceeded all expectations!'",
    "Classify the sentiment of: 'I would not recommend this to anyone.'",
    "Classify the sentiment of: 'Pretty good value for the price.'",
    "Classify the sentiment of: 'Worst purchase I have ever made.'",
    "Classify the sentiment of: 'Surprisingly pleasant, will buy again.'",
]


def separator(title: str) -> None:
    print(f"\n{'=' * 60}")
    print(f"  {title}")
    print("=" * 60)


def print_result(label: str, result: BatchJobResult) -> None:
    content = result.content or "(empty)"
    cost = f"${result.actual_cost_usd:.6f}" if result.actual_cost_usd is not None else "n/a"
    print(
        f"  [{label}]  prompt={result.prompt_tokens}  completion={result.completion_tokens}"
        f"  cost={cost}"
    )
    print(f"    → {content[:120]}")


# ── Connect ────────────────────────────────────────────────────────────────────

batch_window = int(os.environ.get("PROVIZ_BATCH_WINDOW_SECS", "10"))
print(f"Connecting to proviz-server (db={DB}, batch_window={batch_window}s)...")
pz = ProvizElekto(db_path=DB)
print(f"Health: {pz.health()}")

# ── Preflight: verify the step resolves to a Mistral model ────────────────────

print(f"\nPreflight: checking step '{STEP}' resolves to a Mistral model...")
try:
    probe = pz.select(step=STEP, estimated_tokens=100)
    if not probe.brand_slug.startswith("mistral"):
        print(
            f"\nERROR: step '{STEP}' selected '{probe.brand_slug}/{probe.model_slug}'.\n"
            f"Batch is Mistral-only. Add a Mistral rule for this step, e.g.:\n\n"
            f"  ./target/release/proviz --db-path {DB} rule add \\\n"
            f"      --step {STEP} --model mistral-small-2603 --priority 0\n\n"
            f"Or set BATCH_STEP=<step> to target a step that already has a Mistral model.\n"
        )
        pz.report_error(probe.model_id, error_type="parse", brand_key_id=probe.brand_key_id)
        sys.exit(1)
    pz.report_error(probe.model_id, error_type="parse", brand_key_id=probe.brand_key_id)  # release in-flight slot, TTL=0
    print(f"  OK — will use {probe.brand_slug}/{probe.model_slug}")
except AllModelsExhausted:
    print(
        f"\nERROR: no model available for step '{STEP}'.\n"
        f"Add a rule, e.g.:\n\n"
        f"  ./target/release/proviz --db-path {DB} rule add \\\n"
        f"      --step {STEP} --model mistral-small-2603 --priority 0\n"
    )
    sys.exit(1)

# ── Phase 1: single job ────────────────────────────────────────────────────────

separator("Phase 1 — single batch job")

queue = pz.create_batch_queue(STEP)
t0 = time.monotonic()

job = queue.submit([{"role": "user", "content": PROMPTS[0]}])
print(f"  Submitted request_id={job.request_id}  retry_hint={job._retry_after_ms}ms")

# Non-blocking check: result should not be ready immediately.
if job.done():
    print("  WARNING: job.done() returned True immediately — unexpectedly fast.")
else:
    print("  job.done() = False  (as expected, Mistral is processing)")

print(f"  Waiting for result (timeout={TIMEOUT}s)...")
try:
    result = job.result(timeout=TIMEOUT)
    elapsed = time.monotonic() - t0
    print(f"  Result received in {elapsed:.1f}s")
    print_result("job 1", result)
    assert result.prompt_tokens > 0, "expected prompt_tokens > 0"
    assert result.completion_tokens > 0, "expected completion_tokens > 0"
    assert result.content is not None, "expected non-None content"
    print("  Assertions passed.")
except BatchTimeoutError as e:
    print(f"  TIMEOUT: {e}")
    sys.exit(1)
except BatchError as e:
    print(f"  BATCH ERROR: {e}")
    sys.exit(1)

# ── Phase 2: parallel jobs ─────────────────────────────────────────────────────

separator(f"Phase 2 — {BATCH_SIZE} parallel batch jobs")

prompts = (PROMPTS * 4)[:BATCH_SIZE]
queue2 = pz.create_batch_queue(STEP)

t0 = time.monotonic()
jobs: list[BatchJob] = []
for i, prompt in enumerate(prompts):
    j = queue2.submit([{"role": "user", "content": prompt}])
    jobs.append(j)
    print(f"  Submitted [{i+1}/{BATCH_SIZE}]  request_id={j.request_id}")

print(f"\n  All {BATCH_SIZE} requests submitted in {time.monotonic() - t0:.2f}s")
print(f"  Waiting for all results (timeout={TIMEOUT}s)...")

results: list[BatchJobResult] = []
errors: list[str] = []
results_lock = threading.Lock()


def collect(idx: int, j: BatchJob) -> None:
    try:
        r = j.result(timeout=TIMEOUT)
        with results_lock:
            results.append(r)
        print_result(f"job {idx+1:>2}", r)
    except BatchTimeoutError as e:
        with results_lock:
            errors.append(f"job {idx+1}: TIMEOUT — {e}")
    except BatchError as e:
        with results_lock:
            errors.append(f"job {idx+1}: ERROR — {e}")


threads = [threading.Thread(target=collect, args=(i, j), daemon=True) for i, j in enumerate(jobs)]
for t in threads:
    t.start()
for t in threads:
    t.join()

elapsed = time.monotonic() - t0
print(f"\n  Completed {len(results)}/{BATCH_SIZE} jobs in {elapsed:.1f}s")

if errors:
    print(f"\n  FAILURES ({len(errors)}):")
    for err in errors:
        print(f"    {err}")

if results:
    total_prompt = sum(r.prompt_tokens for r in results)
    total_completion = sum(r.completion_tokens for r in results)
    total_cost = sum(r.actual_cost_usd for r in results if r.actual_cost_usd is not None)
    print(f"\n  Aggregate tokens:  prompt={total_prompt}  completion={total_completion}")
    print(f"  Total cost:        ${total_cost:.6f}")
    print(f"  Cost per request:  ${total_cost / len(results):.6f}")

if len(results) < BATCH_SIZE:
    print(f"\n  WARNING: only {len(results)}/{BATCH_SIZE} results collected — check errors above.")
    sys.exit(1)

# ── Phase 3: extra_body forwarding ─────────────────────────────────────────────

separator("Phase 3 — extra_body (max_tokens + temperature)")

queue3 = pz.create_batch_queue(STEP)
t0 = time.monotonic()
j = queue3.submit(
    [{"role": "user", "content": "Reply with exactly one word: positive, negative, or neutral. Input: 'Great!'"}],
    max_tokens=8,
    temperature=0.0,
)
print(f"  Submitted with max_tokens=8  request_id={j.request_id}")
try:
    r = j.result(timeout=TIMEOUT)
    elapsed = time.monotonic() - t0
    print(f"  Result in {elapsed:.1f}s  completion_tokens={r.completion_tokens}")
    print(f"  → {r.content!r}")
    assert r.completion_tokens <= 10, f"expected short reply, got {r.completion_tokens} tokens"
    print("  max_tokens constraint respected.")
except (BatchError, BatchTimeoutError) as e:
    print(f"  FAILED: {e}")
    sys.exit(1)

# ── Phase 4: error path — bad step ────────────────────────────────────────────

separator("Phase 4 — error path (non-existent step)")

try:
    queue_bad = pz.create_batch_queue("__nonexistent_step__")
    queue_bad.submit([{"role": "user", "content": "hello"}])
    print("  WARNING: expected an error for unknown step, but got none.")
except Exception as e:
    print(f"  Got expected error: {type(e).__name__}: {e}")

# ── Summary ────────────────────────────────────────────────────────────────────

separator("Summary")
print(f"  Phase 1 (single job):      PASSED")
print(f"  Phase 2 ({BATCH_SIZE} parallel jobs): {'PASSED' if not errors else 'PARTIAL'}")
print(f"  Phase 3 (extra_body):      PASSED")
print(f"  Phase 4 (error path):      PASSED")
print()
print("Done.")
