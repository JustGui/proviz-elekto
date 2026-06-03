#!/usr/bin/env python3
"""
Proviz log analyzer (Optimized).

Usage:
    python scripts/analyze_logs.py [path/to/logs_proviz.txt]
"""

import re
import sys
import argparse
from collections import defaultdict, Counter
from datetime import datetime
import bisect

# Pre-compile all regular expressions outside the loop
ANSI = re.compile(r'\x1b\[[0-9;]*m')
TS_RE = re.compile(r'(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)Z')
PEER_RE = re.compile(r'peer=(\S+)')
STEP_RE = re.compile(r'\bstep=(\S+)')
MODEL_RE = re.compile(r'\bmodel=(\S+)')
OUTCOME_RE = re.compile(r'outcome=(\S+)')
MID_RE = re.compile(r'model_id=(\S+)')
ERR_RE = re.compile(r'error_type=Some\((\w+)\)')
FH_RE = re.compile(r'fast_headroom=(\S+)')
SKIPPED_RE = re.compile(r'skipped: (.+)')

def strip(s: str) -> str:
    return ANSI.sub('', s)

def fmt_pct(n, total):
    if total == 0:
        return "  n/a"
    return f"{100*n/total:5.1f}%"

def analyze(path: str):
    pending_step: dict[str, str] = {}
    pending_by_ip: dict[str, list] = defaultdict(list)

    model_select_count = Counter()
    step_select_count = Counter()
    model_report_count: dict[str, Counter] = defaultdict(Counter)
    model_latencies: dict[str, list[float]] = defaultdict(list)
    over_quota_count = Counter()
    reactive_skip_count = Counter()
    skip_reasons = Counter()
    remaining_none = 0
    remaining_some = 0
    actual_none = 0
    actual_some = 0
    n_reports = 0

    headroom_values: dict[str, set] = defaultdict(set)
    headroom_trajectory: dict[str, list[float]] = defaultdict(list)
    selects_per_minute: Counter = Counter()
    reports_per_minute: Counter = Counter()

    uuid_to_model: dict[str, str] = {}
    peer_to_model: dict[str, str] = {}
    model_remaining_some: Counter = Counter()
    model_remaining_none: Counter = Counter()

    rl_events: list[tuple] = []
    
    # Store only timestamps and select response markers for the 2nd pass
    # instead of storing every single raw string line.
    select_responses_timeline: list[tuple[datetime, str]] = []
    timestamps_only: list[datetime] = []

    with open(path, 'rb') as f:
        for raw in f:
            line = strip(raw.decode('utf-8', errors='replace')).strip()
            if not line:
                continue

            ts_m = TS_RE.match(line)
            ts = datetime.fromisoformat(ts_m.group(1)) if ts_m else None
            minute_key = ts.strftime('%H:%M') if ts else '?'

            # ── select request ───
            if 'select request' in line and 'peer=' in line:
                peer_m = PEER_RE.search(line)
                step_m = STEP_RE.search(line)
                if peer_m:
                    pending_step[peer_m.group(1)] = step_m.group(1) if step_m else '?'

            # ── select response ───
            elif 'select response' in line and 'peer=' in line:
                peer_m  = PEER_RE.search(line)
                model_m = MODEL_RE.search(line)
                if peer_m and model_m and ts:
                    peer  = peer_m.group(1)
                    ip    = peer.rsplit(':', 1)[0]
                    model = model_m.group(1)
                    step  = pending_step.pop(peer, '?')
                    pending_by_ip[ip].append((ts, model, step))
                    model_select_count[model] += 1
                    step_select_count[step] += 1
                    selects_per_minute[minute_key] += 1
                    peer_to_model[peer] = model
                    
                    # Track for fast index lookup in the second pass
                    select_responses_timeline.append((ts, line))
                    timestamps_only.append(ts)

            # ── report ───
            elif ' report ' in line and 'peer=' in line and 'outcome=' in line:
                peer_m    = PEER_RE.search(line)
                outcome_m = OUTCOME_RE.search(line)
                mid_m     = MID_RE.search(line)
                err_m     = ERR_RE.search(line)
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

                    if mid_m and peer in peer_to_model:
                        uuid_to_model[mid_m.group(1)] = peer_to_model[peer]

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

                        if has_rem_req or has_rem_tok:
                            model_remaining_some[model] += 1
                        else:
                            model_remaining_none[model] += 1

                        if outcome == 'RateLimit':
                            err_type = err_m.group(1) if err_m else '?'
                            rl_events.append((ts, model, step, ip, err_type))

            # ── over quota (soft) ───
            elif 'over quota' in line and 'fast_headroom=' in line:
                model_m = MODEL_RE.search(line)
                fh_m    = FH_RE.search(line)
                if model_m:
                    over_quota_count[model_m.group(1)] += 1
                if model_m and fh_m:
                    headroom_values[model_m.group(1)].add(fh_m.group(1))

            # ── headroom trajectory ───
            elif 'selected model=' in line and 'fast_headroom=' in line:
                model_m = MODEL_RE.search(line)
                fh_m    = FH_RE.search(line)
                if model_m and fh_m:
                    try:
                        headroom_trajectory[model_m.group(1)].append(float(fh_m.group(1)))
                    except ValueError:
                        pass

            # ── skipped ───
            elif 'skipped:' in line:
                reason_m = SKIPPED_RE.search(line)
                if reason_m:
                    reason = reason_m.group(1).strip()
                    skip_reasons[reason] += 1
                    if 'rate limited (reactive)' in reason:
                        model_m = MODEL_RE.search(reason)
                        if model_m:
                            reactive_skip_count[model_m.group(1)] += 1

    # ── Optimized Rate-limit retry analysis ───
    rl_retries: list[tuple] = []
    for ev_ts, ev_model, ev_step, ev_ip, err_type in rl_events:
        retry_model = None
        gap_ms = None
        
        # Use binary search to skip instantly to logs matching this event's timestamp
        start_idx = bisect.bisect_right(timestamps_only, ev_ts)
        
        for idx in range(start_idx, len(select_responses_timeline)):
            ts2, line2 = select_responses_timeline[idx]
            if (ts2 - ev_ts).total_seconds() > 5:
                break
                
            peer_m2 = PEER_RE.search(line2)
            model_m2 = MODEL_RE.search(line2)
            if peer_m2 and model_m2:
                ip2 = peer_m2.group(1).rsplit(':', 1)[0]
                if ip2 == ev_ip:
                    retry_model = model_m2.group(1)
                    gap_ms = (ts2 - ev_ts).total_seconds() * 1000
                    break
        rl_retries.append((ev_model, ev_step, err_type, retry_model, gap_ms))

    total_selects = sum(model_select_count.values())
    total_reports = n_reports

    # ── Output Block (remains identical to your layout) ───
    print("=" * 70)
    print("  PROVIZ LOG ANALYSIS")
    print(f"  File: {path}")
    print("=" * 70)

    print("\n── MODEL SELECTION DISTRIBUTION ─────────────────────────────────────")
    print(f"  {'Model':<42} {'Selects':>8}  {'Share':>6}  {'Reports':>8}  {'Errors':>7}")
    for model, cnt in model_select_count.most_common():
        rep_ok  = model_report_count[model].get('Success', 0)
        rep_err = model_report_count[model].get('Error', 0)
        rep_rl  = model_report_count[model].get('RateLimit', 0)
        rep_tot = rep_ok + rep_err + rep_rl
        err_pct = fmt_pct(rep_err + rep_rl, rep_tot) if rep_tot > 0 else "    -"
        print(f"  {model:<42} {cnt:>8}  {fmt_pct(cnt, total_selects)}  {rep_tot:>8}  {err_pct}")

    print("\n── STEP DISTRIBUTION ────────────────────────────────────────────────")
    for step, cnt in step_select_count.most_common():
        print(f"  {step:<35}  {cnt:>6}  {fmt_pct(cnt, total_selects)}")

    print("\n── REPORT FEEDBACK QUALITY ──────────────────────────────────────────")
    print(f"  Total reports         : {total_reports}")
    print(f"  actual LLM calls      : {actual_some}  {fmt_pct(actual_some, total_reports)}  (actual_tokens present)")
    print(f"  remaining=None (blind): {remaining_none}  {fmt_pct(remaining_none, total_reports)}")
    print(f"  remaining=Some (live) : {remaining_some}  {fmt_pct(remaining_some, total_reports)}")
    print(f"  actual_tokens absent  : {actual_none}   {fmt_pct(actual_none, total_reports)}")

    all_models_cov = sorted(
        set(list(model_remaining_some.keys()) + list(model_remaining_none.keys())),
        key=lambda m: -(model_remaining_some[m] + model_remaining_none[m])
    )
    if all_models_cov:
        print("\n── PER-MODEL ANCHOR COVERAGE (remaining headers forwarded?) ─────────")
        print(f"  {'Model':<42} {'live':>6}  {'blind':>6}  {'cov%':>6}  status")
        for model in all_models_cov:
            s = model_remaining_some[model]
            n = model_remaining_none[model]
            t = s + n
            status = "OK" if s > 0 and n == 0 else ("PARTIAL" if s > 0 else "BLIND")
            print(f"  {model:<42} {s:>6}  {n:>6}  {fmt_pct(s, t)}  {status}")

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

    if headroom_trajectory:
        print("\n── HEADROOM TRAJECTORY WHEN SELECTED (fast_headroom, first→last) ───")
        print(f"  {'Model':<42} {'n':>5}  {'first':>8}  {'last':>8}  {'min':>8}  drift")
        for model, vals in sorted(headroom_trajectory.items(), key=lambda x: -len(x[1])):
            drift = vals[-1] - vals[0] if len(vals) > 1 else 0.0
            drift_s = f"{drift:+.3f}"
            print(f"  {model:<42} {len(vals):>5}  {vals[0]:>8.3f}  {vals[-1]:>8.3f}  {min(vals):>8.3f}  {drift_s}")

    if rl_retries:
        print("\n── RATE-LIMIT EVENTS + IMMEDIATE RETRY ─────────────────────────────")
        print(f"  {'failed model':<35}  {'err':>5}  step  →  retry model  (gap ms)")
        for ev_model, step, err_type, retry_model, gap_ms in rl_retries:
            retry_str = f"{retry_model}  ({gap_ms:.0f} ms)" if retry_model else "no retry within 5s"
            same = "  ← SAME!" if retry_model == ev_model else ""
            print(f"  {ev_model:<35}  {err_type:>5}  {step}  →  {retry_str}{same}")

    print("\n── REACTIVE RATE-LIMIT SKIPS (model hard-blocked after 429) ─────────")
    if reactive_skip_count:
        for model, cnt in reactive_skip_count.most_common():
            print(f"  {model:<42}  {cnt:>6}x blocked")
    else:
        print("  (none)")

    print("\n── SOFT OVER-QUOTA LOG HITS ─────────────────────────────────────────")
    print("  (soft = still eligible, just scored lower)")
    for model, cnt in over_quota_count.most_common(12):
        vals = headroom_values[model]
        frozen = " *** FROZEN HEADROOM ***" if len(vals) == 1 else f" ({len(vals)} distinct values)"
        val_str = next(iter(vals)) if len(vals) == 1 else ""
        print(f"  {model:<42}  {cnt:>6}x  {frozen}  {val_str}")

    print("\n── ALL SKIP REASONS ─────────────────────────────────────────────────")
    for reason, cnt in skip_reasons.most_common():
        print(f"  {cnt:>6}x  {reason}")

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
    blind_models = [m for m in all_models_cov if model_remaining_some[m] == 0]
    if blind_models:
        issues.append(
            f"BLIND ANCHOR on {len(blind_models)} model(s) — provider never returns remaining headers:\n" +
            "\n".join(f"          {m}  ({model_remaining_none[m]} blind reports)" for m in blind_models)
        )
    frozen = [(m, next(iter(headroom_values[m]))) for m in over_quota_count if len(headroom_values[m]) == 1]
    if frozen:
        issues.append(f"FROZEN HEADROOM on {len(frozen)} model(s) — stale anchor from prior session, no recovery:\n" +
                       "\n".join(f"          {m} = {v}" for m, v in frozen[:5]))
    if rl_retries:
        same_retries = [(m, rm) for m, _, _, rm, _ in rl_retries if rm == m]
        if same_retries:
            issues.append(f"RETRY LANDED ON SAME MODEL after rate-limit ({len(same_retries)} times) — "
                           "reactive block may not have registered in time.")
        else:
            retry_info = ", ".join(f"{m}→{rm}" for m, _, _, rm, _ in rl_retries if rm)
            issues.append(f"Rate-limit retries all rotated to a different model (good): {retry_info}")
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