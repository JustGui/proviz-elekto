#!/usr/bin/env python3
"""
Proviz log analyzer.

Usage:
    python scripts/analyze_logs.py [path/to/logs_proviz.txt]

If no path is given, looks for logs_proviz.txt in the current directory.
"""

import re
import sys
import argparse
from collections import defaultdict, Counter
from datetime import datetime, timedelta

ANSI = re.compile(r'\x1b\[[0-9;]*m')

def strip(s: str) -> str:
    return ANSI.sub('', s)

def parse_kv(line: str) -> dict:
    """Parse key=value pairs from a stripped log line."""
    return dict(re.findall(r'(\w+)=(\S+)', line))

def fmt_pct(n, total):
    if total == 0:
        return "  n/a"
    return f"{100*n/total:5.1f}%"

def analyze(path: str):
    # Per-peer select tracking:
    #   peer -> step  (from select request, keyed by IP:port)
    pending_step: dict[str, str] = {}
    #   IP -> deque of (ts, model, step) ordered by time (for report latency matching)
    pending_by_ip: dict[str, list] = defaultdict(list)

    model_select_count = Counter()
    step_select_count = Counter()
    model_report_count: dict[str, Counter] = defaultdict(Counter)  # model -> outcome -> count
    model_latencies: dict[str, list[float]] = defaultdict(list)    # model -> [seconds]
    over_quota_count = Counter()         # model -> count
    reactive_skip_count = Counter()      # model -> count
    skip_reasons = Counter()             # reason -> count
    remaining_none = 0
    remaining_some = 0
    actual_none = 0
    actual_some = 0
    n_reports = 0

    # Frozen headroom detection: model -> set of unique headroom values seen
    headroom_values: dict[str, set] = defaultdict(set)

    # Timeline: bucket by minute
    selects_per_minute: Counter = Counter()
    reports_per_minute: Counter = Counter()

    with open(path, 'rb') as f:
        for raw in f:
            line = strip(raw.decode('utf-8', errors='replace')).strip()
            if not line:
                continue

            ts_m = re.match(r'(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)Z', line)
            ts = datetime.fromisoformat(ts_m.group(1)) if ts_m else None
            minute_key = ts.strftime('%H:%M') if ts else '?'

            # ── select request (step lives here) ─────────────────────────────
            if 'select request' in line and 'peer=' in line:
                peer_m = re.search(r'peer=(\S+)', line)
                step_m = re.search(r'\bstep=(\S+)', line)
                if peer_m:
                    pending_step[peer_m.group(1)] = step_m.group(1) if step_m else '?'

            # ── select response ──────────────────────────────────────────────
            elif 'select response' in line and 'peer=' in line:
                peer_m  = re.search(r'peer=(\S+)', line)
                model_m = re.search(r'\bmodel=(\S+)', line)
                if peer_m and model_m and ts:
                    peer  = peer_m.group(1)
                    ip    = peer.rsplit(':', 1)[0]
                    model = model_m.group(1)
                    step  = pending_step.pop(peer, '?')
                    pending_by_ip[ip].append((ts, model, step))
                    model_select_count[model] += 1
                    step_select_count[step] += 1
                    selects_per_minute[minute_key] += 1

            # ── report ───────────────────────────────────────────────────────
            elif ' report ' in line and 'peer=' in line and 'outcome=' in line:
                peer_m    = re.search(r'peer=(\S+)', line)
                outcome_m = re.search(r'outcome=(\S+)', line)
                n_reports += 1

                has_rem_req = 'remaining_requests=Some' in line
                has_rem_tok = 'remaining_tokens=Some' in line
                has_act     = 'actual_tokens=Some' in line
                if has_rem_req or has_rem_tok:
                    remaining_some += 1
                else:
                    remaining_none += 1
                if has_act:
                    actual_some += 1
                else:
                    actual_none += 1

                if peer_m and ts:
                    peer    = peer_m.group(1)
                    ip      = peer.rsplit(':', 1)[0]
                    outcome = outcome_m.group(1) if outcome_m else '?'
                    reports_per_minute[minute_key] += 1

                    # Match to nearest unconsumed select from same IP before this report
                    queue = pending_by_ip.get(ip, [])
                    best_idx = None
                    for idx, (sel_ts, model, step) in enumerate(queue):
                        if sel_ts <= ts:
                            best_idx = idx
                    if best_idx is not None:
                        sel_ts, model, step = queue.pop(best_idx)
                        dt = (ts - sel_ts).total_seconds()
                        model_report_count[model][outcome] += 1
                        if 0 < dt < 600:
                            model_latencies[model].append(dt)

            # ── over quota (soft) ────────────────────────────────────────────
            elif 'over quota' in line and 'fast_headroom=' in line:
                model_m = re.search(r'model=(\S+)', line)
                fh_m    = re.search(r'fast_headroom=(\S+)', line)
                if model_m:
                    over_quota_count[model_m.group(1)] += 1
                if model_m and fh_m:
                    headroom_values[model_m.group(1)].add(fh_m.group(1))

            # ── skipped ──────────────────────────────────────────────────────
            elif 'skipped:' in line:
                reason_m = re.search(r'skipped: (.+)', line)
                if reason_m:
                    reason = reason_m.group(1).strip()
                    skip_reasons[reason] += 1
                    if 'rate limited (reactive)' in reason:
                        model_m = re.search(r'model=(\S+)', reason)
                        if model_m:
                            reactive_skip_count[model_m.group(1)] += 1

    total_selects = sum(model_select_count.values())
    total_reports = n_reports

    print("=" * 70)
    print("  PROVIZ LOG ANALYSIS")
    print(f"  File: {path}")
    print("=" * 70)

    # ── Model selection distribution ─────────────────────────────────────────
    print("\n── MODEL SELECTION DISTRIBUTION ─────────────────────────────────────")
    print(f"  {'Model':<42} {'Selects':>8}  {'Share':>6}  {'Reports':>8}  {'Errors':>7}")
    for model, cnt in model_select_count.most_common():
        rep_ok  = model_report_count[model].get('Success', 0)
        rep_err = model_report_count[model].get('Error', 0)
        rep_rl  = model_report_count[model].get('RateLimit', 0)
        rep_tot = rep_ok + rep_err + rep_rl
        err_pct = fmt_pct(rep_err + rep_rl, rep_tot) if rep_tot > 0 else "    -"
        print(f"  {model:<42} {cnt:>8}  {fmt_pct(cnt, total_selects)}  {rep_tot:>8}  {err_pct}")

    # ── Step distribution ────────────────────────────────────────────────────
    print("\n── STEP DISTRIBUTION ────────────────────────────────────────────────")
    for step, cnt in step_select_count.most_common():
        print(f"  {step:<35}  {cnt:>6}  {fmt_pct(cnt, total_selects)}")

    # ── Feedback quality ─────────────────────────────────────────────────────
    print("\n── REPORT FEEDBACK QUALITY ──────────────────────────────────────────")
    print(f"  Total reports         : {total_reports}")
    print(f"  remaining=None (blind): {remaining_none}  {fmt_pct(remaining_none, total_reports)}")
    print(f"  remaining=Some (live) : {remaining_some}  {fmt_pct(remaining_some, total_reports)}")
    print(f"  actual_tokens absent  : {actual_none}   {fmt_pct(actual_none, total_reports)}")

    # ── LLM call latencies ───────────────────────────────────────────────────
    print("\n── LLM CALL LATENCIES (select → report, seconds) ───────────────────")
    print(f"  {'Model':<42} {'n':>5}  {'p50':>6}  {'p95':>6}  {'p99':>6}  {'max':>6}")
    for model, lats in sorted(model_latencies.items(), key=lambda x: -len(x[1])):
        if not lats:
            continue
        s = sorted(lats)
        p50 = s[max(0, len(s)//2 - 1)]
        p95 = s[min(len(s)-1, int(len(s)*0.95))]
        p99 = s[min(len(s)-1, int(len(s)*0.99))]
        print(f"  {model:<42} {len(s):>5}  {p50:>6.1f}  {p95:>6.1f}  {p99:>6.1f}  {max(s):>6.1f}")

    # ── Reactive rate-limit skips ────────────────────────────────────────────
    print("\n── REACTIVE RATE-LIMIT SKIPS (model hard-blocked after 429) ─────────")
    for model, cnt in reactive_skip_count.most_common():
        print(f"  {model:<42}  {cnt:>6}x blocked")

    # ── Persistent over-quota (soft) ─────────────────────────────────────────
    print("\n── SOFT OVER-QUOTA LOG HITS ─────────────────────────────────────────")
    print("  (soft = still eligible, just scored lower)")
    for model, cnt in over_quota_count.most_common(12):
        vals = headroom_values[model]
        frozen = " *** FROZEN HEADROOM ***" if len(vals) == 1 else f" ({len(vals)} distinct values)"
        val_str = next(iter(vals)) if len(vals) == 1 else ""
        print(f"  {model:<42}  {cnt:>6}x  {frozen}  {val_str}")

    # ── Skip reasons ─────────────────────────────────────────────────────────
    print("\n── ALL SKIP REASONS ─────────────────────────────────────────────────")
    for reason, cnt in skip_reasons.most_common():
        print(f"  {cnt:>6}x  {reason}")

    # ── Throughput timeline ──────────────────────────────────────────────────
    print("\n── THROUGHPUT BY MINUTE (selects / reports) ─────────────────────────")
    all_minutes = sorted(set(list(selects_per_minute.keys()) + list(reports_per_minute.keys())))
    for m in all_minutes:
        s = selects_per_minute.get(m, 0)
        r = reports_per_minute.get(m, 0)
        bar = '█' * (s // 10)
        print(f"  {m}  sel={s:4d} rep={r:4d}  {bar}")

    print("\n" + "=" * 70)
    print("  SUMMARY / ISSUES DETECTED")
    print("=" * 70)

    issues = []
    if remaining_some == 0:
        issues.append("CRITICAL: No report ever includes remaining_requests/remaining_tokens.\n"
                       "          The anchor feature is completely blind — proviz cannot correct\n"
                       "          quota drift from provider reality.")
    frozen = [(m, next(iter(headroom_values[m]))) for m in over_quota_count if len(headroom_values[m]) == 1]
    if frozen:
        issues.append(f"FROZEN HEADROOM on {len(frozen)} model(s):\n" +
                       "\n".join(f"          {m} = {v}" for m, v in frozen[:5]))
    if reactive_skip_count:
        top = reactive_skip_count.most_common(3)
        issues.append("Reactive rate-limit blocks detected (models hit real 429s):\n" +
                       "\n".join(f"          {m}: {c}x" for m, c in top))
    if not issues:
        print("  No major issues detected.")
    for i, issue in enumerate(issues, 1):
        print(f"\n  [{i}] {issue}")
    print()


def main():
    ap = argparse.ArgumentParser(description="Analyze proviz log files.")
    ap.add_argument('path', nargs='?', default='logs_proviz.txt')
    args = ap.parse_args()
    analyze(args.path)


if __name__ == '__main__':
    main()
