# Changelog

## 0.24.0 — 2026-10-08

- Add opt-in group/request latency ceilings before scoring, with fast overflow and a logged fastest-available fallback.
- Persist decaying size-aware latency samples per model/key across restarts.
- Add atomic per-model/key concurrency caps and preference for capped prepaid endpoints.
- Learn unknown RPM limits after 429s; honour Retry-After and organisation-wide quota failures.
- Expose candidate score components in DEBUG logs and model latency/429/in-flight metrics.
- Add explicit pinned-call waiting, CLI configuration, Python support, and SQLite/PostgreSQL migrations.
