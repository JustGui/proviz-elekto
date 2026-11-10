# Changelog

## Unreleased

- Classify provider failures (`credits_exhausted`, `quota`, `rate_limited`, `auth`, …) and return the reason on `/complete` 502/409 bodies.
- Add Prometheus `GET /metrics` with per-brand/model/step/group call, failure-reason and `/complete` result counters.
- Stop forwarding provider error bodies in `detail` or logs (status, reason and provider code only); stop logging request URLs on connection errors.
- Back the whole key off when a provider reports an empty balance.

## 0.24.0 — 2026-10-08

- Add opt-in group/request latency ceilings before scoring, with fast overflow and a logged fastest-available fallback.
- Persist decaying size-aware latency samples per model/key across restarts.
- Add atomic per-model/key concurrency caps and preference for capped prepaid endpoints.
- Learn unknown RPM limits after 429s; honour Retry-After and organisation-wide quota failures.
- Expose candidate score components in DEBUG logs and model latency/429/in-flight metrics.
- Add explicit pinned-call waiting, CLI configuration, Python support, and SQLite/PostgreSQL migrations.
