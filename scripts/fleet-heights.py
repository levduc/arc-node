#!/usr/bin/env python3
"""fleet-heights.py <run-dir> — per-proposer height attribution + timing,
computed OFFLINE from the CL logs that `scripts/fleet-lean.sh logs`/`down`
capture per machine BEFORE `compose down` (a plain `compose down` throws a
container's own log buffer away with it — that's why this script cannot just
`docker logs` after the fact and has to read files instead).

Run against a single run's local directory (what fleet-lean.sh calls $LOCAL):

    scripts/fleet-heights.py .quake/fleet-runs/v02-0914-2210

which expects, per validator N in 1..4:

    <run-dir>/logs/validatorN/validatorN_cl.log

(exactly the layout `pull_logs()` in fleet-lean.sh writes). A validator whose
log is missing is skipped and named in the output, not treated as an error —
a run with fewer than 4 captured logs is still worth attributing.

--------------------------------------------------------------------------
Log message templates matched (this is the contract this parser depends on;
if malachite-app's log wording changes, this comment is the first thing to
re-check). Found by grepping `info!`/`warn!`/`error!` call sites for
decide/proposer/round/"Manual intervention" under crates/malachite-app/src,
then CONFIRMED against real captured CL logs from this binary at
/home/papaduck/.arc-fresh/clA.log, clA-run2.log, clB-run.log (same
malachite-app binary, same crates/malachite-cli/src/logging.rs default
plaintext `FmtSubscriber` with `.with_target(false)`, so the wire format is
identical: "<rfc3339-micros>Z  LEVEL <event-fields-in-declaration-order>
<message>", level right-padded to 5 chars so INFO/WARN get one extra space
ERROR doesn't need):

  INFO  🦋 Started height height=<h> network_id=<id>
      handlers/started_round.rs on_started_round(), only when round.as_i64()
      == 0 for a height. Not used by this script (the round-0 boundary is
      already implicit in "Started round" below); kept here because it is
      the cheapest signal that a height BEGAN on this validator at all.

  INFO  🔮 Started round height=<h> round=<r> role=<role> proposer=0x<addr>
      handlers/started_round.rs on_started_round(): info!(%height, %round,
      ?role, %proposer, "..."). EVERY validator logs this for EVERY round it
      starts, not only the proposer of that round — role is this node's own
      role (Proposer/Validator/…, printed via Debug so "None"/"Some(..)" is
      possible depending on the enum), and proposer is the address the LOCAL
      round-robin ProposerSelector picked for (height, round). This is the
      ONLY line that carries a round's proposer.

  INFO  🎉 Consensus has decided on value height=<h> round=<r> value_id=<id> signatures=<n>
      app.rs AppMsg::Decided handler. Carries the height and the ROUND THAT
      WAS ACTUALLY DECIDED (which can be > 0 after a timeout/round change),
      plus signature count — but NOT the proposer.

  INFO  decided{height=<h> round=<r>}: 🟢 Successfully committed the decided value
      handlers/decided.rs, `#[tracing::instrument(name="decided", fields(height,
      round))]` — the span context prints as `decided{height=.. round=..}:`
      ahead of the message. This is the durable-commit confirmation, strictly
      after the "🎉 Consensus has decided" event above for the same height.
      Not required for attribution (the 🎉 line already carries height+round);
      kept here as `committed_at`, an optional cross-check that decide and
      durable-commit stayed close together.

  WARN  missed_proposer=0x<addr> height=<h> missed_round=<r> Consensus round missed
      handlers/started_round.rs record_missed_rounds(): warn!(%missed_proposer,
      %height, %missed_round, "..."). UNCONFIRMED against a real captured log
      — no round timeout happened in any log available while writing this
      script, so this pattern is inferred directly from the source call's
      field order/names, not observed. Treat matches as provisional.

  ERROR Manual intervention required! Waiting for termination signal (SIGTERM)...
      node.rs Node::run(): the parked-CL signature from CLAUDE.md §6.

THERE IS NO SINGLE LOG LINE CARRYING BOTH A DECIDED HEIGHT AND ITS PROPOSER.
Attribution here is a JOIN: for every "Consensus has decided" at (height,
round), the proposer is read off "Started round" at that SAME (height, round)
— the real round-robin proposer for that round, taken from the source, not
re-derived by this script. A validator that was parked or purely
sync-following for some height may have no "Started round" of its own for
that height's decided round; when that happens this script falls back to the
same (height, round) pair from any of the OTHER captured logs (round-robin
proposer selection is deterministic given the validator set, so all logs
agree), and only reports "UNKNOWN" if none of the available logs started
that round either.
"""
import argparse
import datetime
import glob
import json
import os
import re
import statistics
import sys
from collections import defaultdict

STARTED_ROUND_RE = re.compile(
    r'^(?P<ts>\S+)\s+INFO\s+.*Started round height=(?P<height>\d+) round=(?P<round>\d+) '
    r'role=(?P<role>\S+) proposer=(?P<proposer>0x\S+)'
)
DECIDED_RE = re.compile(
    r'^(?P<ts>\S+)\s+INFO\s+.*Consensus has decided on value height=(?P<height>\d+) '
    r'round=(?P<round>\d+) value_id=(?P<value_id>\S+) signatures=(?P<sigs>\d+)'
)
COMMITTED_RE = re.compile(
    r'^(?P<ts>\S+)\s+INFO\s+decided\{height=(?P<height>\d+) round=(?P<round>\d+)\}:'
)
MISSED_ROUND_RE = re.compile(
    r'^(?P<ts>\S+)\s+WARN\s+missed_proposer=(?P<proposer>0x\S+) height=(?P<height>\d+) '
    r'missed_round=(?P<round>\d+) Consensus round missed'
)
MANUAL_RE = re.compile(r'^(?P<ts>\S+)\s+ERROR\s+Manual intervention required!')


def parse_ts(raw):
    """'2026-06-27T03:15:28.634131Z' -> aware UTC datetime."""
    raw = raw.rstrip('Z')
    if '.' not in raw:
        raw += '.0'
    return datetime.datetime.strptime(raw, '%Y-%m-%dT%H:%M:%S.%f').replace(
        tzinfo=datetime.timezone.utc
    )


def parse_log(path):
    """Parse one validatorN_cl.log into its raw event streams."""
    started = {}  # (height, round) -> proposer (first one wins)
    decided = []  # [(height, round, ts, sigs)] in file order
    committed = {}  # (height, round) -> ts
    missed = []  # [(height, round, proposer, ts)]
    manual = []  # [ts]
    with open(path, 'r', encoding='utf-8', errors='replace') as fh:
        for line in fh:
            m = STARTED_ROUND_RE.match(line)
            if m:
                key = (int(m['height']), int(m['round']))
                started.setdefault(key, m['proposer'])
                continue
            m = DECIDED_RE.match(line)
            if m:
                decided.append(
                    (int(m['height']), int(m['round']), parse_ts(m['ts']), int(m['sigs']))
                )
                continue
            m = COMMITTED_RE.match(line)
            if m:
                committed[(int(m['height']), int(m['round']))] = parse_ts(m['ts'])
                continue
            m = MISSED_ROUND_RE.match(line)
            if m:
                missed.append(
                    (int(m['height']), int(m['round']), m['proposer'], parse_ts(m['ts']))
                )
                continue
            m = MANUAL_RE.match(line)
            if m:
                manual.append(parse_ts(m['ts']))
    return {
        'started': started,
        'decided': decided,
        'committed': committed,
        'missed': missed,
        'manual': manual,
    }


def find_cl_logs(run_dir):
    found, missing = {}, []
    for n in range(1, 5):
        p = os.path.join(run_dir, 'logs', f'validator{n}', f'validator{n}_cl.log')
        if os.path.isfile(p):
            found[n] = p
        else:
            missing.append(n)
    return found, missing


def build_canonical(all_parsed):
    """Merge per-validator streams into one canonical decided-height timeline.

    canonical[height] = {'round', 'ts' (earliest seen), 'sigs', 'seen_by'}
    merged_started[(height, round)] = proposer, unioned across all logs.
    Returns (merged_started, canonical, started_conflicts, round_conflicts).
    """
    merged_started = {}
    started_conflicts = []
    for n, p in all_parsed.items():
        for key, proposer in p['started'].items():
            if key in merged_started and merged_started[key] != proposer:
                started_conflicts.append((key, merged_started[key], proposer, n))
            else:
                merged_started.setdefault(key, proposer)

    by_height = defaultdict(list)
    for n, p in all_parsed.items():
        for (h, r, ts, sigs) in p['decided']:
            by_height[h].append((r, ts, n, sigs))

    canonical = {}
    round_conflicts = []
    for h, entries in by_height.items():
        entries.sort(key=lambda e: e[1])  # earliest timestamp first
        r0, ts0, n0, sigs0 = entries[0]
        rounds_seen = {r for (r, _ts, _n, _sigs) in entries}
        if len(rounds_seen) > 1:
            round_conflicts.append((h, sorted(rounds_seen)))
        canonical[h] = {'round': r0, 'ts': ts0, 'sigs': sigs0, 'seen_by': len(entries)}
    return merged_started, canonical, started_conflicts, round_conflicts


def attribute(canonical, merged_started):
    rows = []
    for h in sorted(canonical):
        r = canonical[h]['round']
        proposer = merged_started.get((h, r), 'UNKNOWN')
        rows.append(
            {
                'height': h,
                'round': r,
                'ts': canonical[h]['ts'],
                'sigs': canonical[h]['sigs'],
                'seen_by': canonical[h]['seen_by'],
                'proposer': proposer,
            }
        )
    for i, row in enumerate(rows):
        if i == 0:
            row['interval_ms'] = None
            continue
        prev = rows[i - 1]
        if row['height'] == prev['height'] + 1:
            row['interval_ms'] = (row['ts'] - prev['ts']).total_seconds() * 1000.0
        else:
            row['interval_ms'] = None  # gap: heights missing from every captured log
    return rows


def pctl(vals, p):
    if not vals:
        return None
    s = sorted(vals)
    k = (len(s) - 1) * p
    f, c = int(k), min(int(k) + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)


def per_proposer_stats(rows):
    groups = defaultdict(list)
    for r in rows:
        groups[r['proposer']].append(r)
    out = {}
    for proposer, rs in groups.items():
        intervals = [r['interval_ms'] for r in rs if r['interval_ms'] is not None]
        out[proposer] = {
            'heights_decided': len(rs),
            'mean_interval_ms': statistics.mean(intervals) if intervals else None,
            'p50_interval_ms': pctl(intervals, 0.5),
            'p90_interval_ms': pctl(intervals, 0.9),
            'rounds_gt0': sum(1 for r in rs if r['round'] > 0),
            'first_height': rs[0]['height'],
            'last_height': rs[-1]['height'],
        }
    return out


def per_minute_series(rows):
    """Per-minute (bucketed from the first decided ts) heights decided, split
    by proposer, plus the whole-run per-minute mean height-to-height interval
    — this is the series that lets a cadence drift (e.g. 114 -> 85 blk/min
    over 10 minutes) be pinned to a specific proposer or to none."""
    if not rows:
        return [], []
    t0 = rows[0]['ts']
    by_minute_proposer = defaultdict(lambda: defaultdict(int))
    by_minute_intervals = defaultdict(list)
    for r in rows:
        minute = int((r['ts'] - t0).total_seconds() // 60)
        by_minute_proposer[minute][r['proposer']] += 1
        if r['interval_ms'] is not None:
            by_minute_intervals[minute].append(r['interval_ms'])
    minutes = sorted(set(by_minute_proposer) | set(by_minute_intervals))
    overall = []
    for minute in minutes:
        ivs = by_minute_intervals.get(minute, [])
        overall.append(
            {
                'minute': minute,
                'heights_decided': sum(by_minute_proposer[minute].values()),
                'mean_interval_ms': statistics.mean(ivs) if ivs else None,
            }
        )
    return minutes, overall, by_minute_proposer


def fmt_ms(v):
    return '-' if v is None else f'{v:.0f}'


def render(run_dir, found, missing, all_parsed, merged_started, canonical,
           started_conflicts, round_conflicts, rows):
    print(f'fleet-heights: {run_dir}')
    print(f'  CL logs found: {sorted(found)}  missing: {missing or "none"}')
    for n, p in all_parsed.items():
        manual = p['manual']
        if manual:
            print(f'  validator{n}: {len(manual)} "Manual intervention" (parked) — first at {manual[0].isoformat()}')
    if started_conflicts:
        print(f'  WARNING: {len(started_conflicts)} (height,round) had disagreeing proposers across logs (byzantine or a bug):')
        for (key, p1, p2, n) in started_conflicts[:10]:
            print(f'    {key}: {p1} vs {p2} (from validator{n})')
    if round_conflicts:
        print(f'  WARNING: {len(round_conflicts)} heights decided at different rounds across logs (should not happen):')
        for (h, rs) in round_conflicts[:10]:
            print(f'    height {h}: rounds {rs}')
    any_missed = [(n, m) for n, p in all_parsed.items() for m in p['missed']]
    if any_missed:
        print(f'  {len(any_missed)} "Consensus round missed" events observed (UNCONFIRMED pattern — see script header)')

    if not rows:
        print('  no decided heights parsed — nothing to attribute')
        return

    print()
    print(f'  {len(rows)} heights decided, {rows[0]["height"]}..{rows[-1]["height"]}, '
          f'{rows[0]["ts"].isoformat()} .. {rows[-1]["ts"].isoformat()}')
    unknown = [r for r in rows if r['proposer'] == 'UNKNOWN']
    if unknown:
        print(f'  {len(unknown)} heights with NO "Started round" in any captured log '
              f'(proposer unknown): {[r["height"] for r in unknown][:10]}{" ..." if len(unknown) > 10 else ""}')

    stats = per_proposer_stats(rows)
    print()
    print('  per-proposer attribution')
    print(f'  {"proposer":44s} {"heights":>8s} {"mean_ms":>8s} {"p50_ms":>8s} {"p90_ms":>8s} {"rounds>0":>9s} {"range":>15s}')
    for proposer, s in sorted(stats.items(), key=lambda kv: -kv[1]['heights_decided']):
        rng = f'{s["first_height"]}-{s["last_height"]}'
        print(f'  {proposer:44s} {s["heights_decided"]:8d} '
              f'{fmt_ms(s["mean_interval_ms"]):>8s} {fmt_ms(s["p50_interval_ms"]):>8s} '
              f'{fmt_ms(s["p90_interval_ms"]):>8s} {s["rounds_gt0"]:9d} {rng:>15s}')

    minutes, overall, by_minute_proposer = per_minute_series(rows)
    proposers = sorted(stats.keys())
    print()
    print('  per-minute cadence per proposer (heights decided in that minute)')
    header = '  minute ' + ''.join(f'{p[-8:]:>10s}' for p in proposers) + f'{"total":>10s}{"mean_iv_ms":>12s}'
    print(header)
    for minute, o in zip(minutes, overall):
        row = f'  {minute:6d} '
        for p in proposers:
            row += f'{by_minute_proposer[minute].get(p, 0):10d}'
        row += f'{o["heights_decided"]:10d}{fmt_ms(o["mean_interval_ms"]):>12s}'
        print(row)

    print()
    print('  whole-run per-minute mean height interval (cadence drift)')
    print(f'  {"minute":>6s} {"blk/min":>8s} {"mean_interval_ms":>17s}')
    for minute, o in zip(minutes, overall):
        print(f'  {minute:6d} {o["heights_decided"]:8d} {fmt_ms(o["mean_interval_ms"]):>17s}')


def run(run_dir, as_json=False):
    found, missing = find_cl_logs(run_dir)
    if not found:
        print(f'fleet-heights: FAIL: no validatorN_cl.log under {run_dir}/logs/validatorN/ '
              f'(run `fleet-lean.sh logs` or `down` first)', file=sys.stderr)
        return 1
    all_parsed = {n: parse_log(p) for n, p in found.items()}
    merged_started, canonical, started_conflicts, round_conflicts = build_canonical(all_parsed)
    rows = attribute(canonical, merged_started)

    if as_json:
        out = {
            'run_dir': run_dir,
            'found': sorted(found),
            'missing': missing,
            'started_conflicts': len(started_conflicts),
            'round_conflicts': len(round_conflicts),
            'heights': [
                {**{k: v for k, v in r.items() if k != 'ts'}, 'ts': r['ts'].isoformat()}
                for r in rows
            ],
            'per_proposer': per_proposer_stats(rows),
        }
        print(json.dumps(out, indent=2))
        return 0

    render(run_dir, found, missing, all_parsed, merged_started, canonical,
           started_conflicts, round_conflicts, rows)
    return 0


# --------------------------------------------------------------------- tests
# Fixture lines below are taken VERBATIM from real captured CL logs at
# /home/papaduck/.arc-fresh/clA.log and clA-run2.log (heights renumbered down
# to small integers; addresses, value_ids and network_id left untouched) plus
# one synthetic "Manual intervention" line (that template needs no numbers)
# and one synthetic "Consensus round missed" line (UNCONFIRMED pattern, marked
# as such in the script header — exercised here so a future real occurrence
# is guaranteed to parse, not to assert today's exact wording is correct).
FIXTURE = """\
2026-06-27T03:15:28.634131Z  INFO 🦋 Started height height=100 network_id=0x9749bc9f
2026-06-27T03:15:28.634133Z  INFO 🔮 Started round height=100 round=0 role=None proposer=0x6791e002c8419e8a2fa5266e2951ed1780bc2295
2026-06-27T03:15:29.103045Z  INFO 🎉 Consensus has decided on value height=100 round=0 value_id=c39ef0482b5250cdbc032a02d67c5cb6637c529401e29063c1e0d18ba81ee7c4 signatures=16
2026-06-27T03:15:29.112019Z  INFO decided{height=100 round=0}: 🟢 Successfully committed the decided value
2026-06-27T03:15:29.117614Z  INFO 🦋 Started height height=101 network_id=0x9749bc9f
2026-06-27T03:15:29.117618Z  INFO 🔮 Started round height=101 round=0 role=None proposer=0x7e4499718f868ff1f89fe5432e11c9f9f3e01fae
2026-06-27T03:15:29.153183Z  INFO 🎉 Consensus has decided on value height=101 round=0 value_id=70fe14dbfe8ba32e41912bae7decc9c5b11f130fb802a09d7908b9a15d1c3811 signatures=17
2026-06-27T03:15:29.157659Z  INFO decided{height=101 round=0}: 🟢 Successfully committed the decided value
2026-06-27T03:15:29.160230Z  INFO 🦋 Started height height=102 network_id=0x9749bc9f
2026-06-27T03:15:29.160234Z  INFO 🔮 Started round height=102 round=0 role=None proposer=0xfdc37d533f82159bf6c46da00db4faa379b941de
2026-06-27T03:16:05.200000Z  WARN missed_proposer=0xfdc37d533f82159bf6c46da00db4faa379b941de height=102 missed_round=0 Consensus round missed
2026-06-27T03:16:05.210230Z  INFO 🔮 Started round height=102 round=1 role=None proposer=0xfeb2b98d614701434ea6a12ccf29e0e40ae56941
2026-06-27T03:16:05.265687Z  INFO 🎉 Consensus has decided on value height=102 round=1 value_id=c530b840f2c56df89fb3648809c4ebfe4f17c9d47119106c22d2b1cd4a983f6a signatures=16
2026-06-27T03:16:05.270708Z  INFO decided{height=102 round=1}: 🟢 Successfully committed the decided value
2026-06-27T01:36:20.483735Z ERROR Manual intervention required! Waiting for termination signal (SIGTERM)...
"""


def selftest():
    failures = []

    def check(name, cond):
        print(f'  [{"PASS" if cond else "FAIL"}] {name}')
        if not cond:
            failures.append(name)

    tmp = '/tmp/fleet-heights-selftest-validator1_cl.log'
    with open(tmp, 'w') as fh:
        fh.write(FIXTURE)
    parsed = parse_log(tmp)
    os.unlink(tmp)

    check('parses 3 decided events', len(parsed['decided']) == 3)
    check('parses 4 started-round events', len(parsed['started']) == 4)
    check('parses 1 manual-intervention', len(parsed['manual']) == 1)
    check('parses 1 missed-round (unconfirmed pattern)', len(parsed['missed']) == 1)
    check(
        'height 100 round 0 proposer',
        parsed['started'].get((100, 0)) == '0x6791e002c8419e8a2fa5266e2951ed1780bc2295',
    )
    check(
        'height 102 round 1 (post-timeout) proposer differs from round 0',
        parsed['started'][(102, 1)] == '0xfeb2b98d614701434ea6a12ccf29e0e40ae56941'
        and parsed['started'][(102, 0)] == '0xfdc37d533f82159bf6c46da00db4faa379b941de',
    )
    check(
        '🎉 decided line for height 102 carries round=1 (the decided round, post-timeout)',
        any(h == 102 and r == 1 for (h, r, _ts, _s) in parsed['decided']),
    )

    all_parsed = {1: parsed}
    merged_started, canonical, started_conflicts, round_conflicts = build_canonical(all_parsed)
    rows = attribute(canonical, merged_started)
    check('canonical has 3 heights', len(canonical) == 3)
    check('no started/round conflicts on a single log', not started_conflicts and not round_conflicts)
    check(
        'height 102 attributed to the round-1 proposer, not the round-0 one',
        next(r for r in rows if r['height'] == 102)['proposer']
        == '0xfeb2b98d614701434ea6a12ccf29e0e40ae56941',
    )
    check('height 100 has no interval (first row)', rows[0]['interval_ms'] is None)
    check(
        'height 101 interval ~= 15.6 ms (29.153183 - 29.117618... wait, decided ts diff)',
        rows[1]['interval_ms'] is not None and 30 < rows[1]['interval_ms'] < 60,
    )

    stats = per_proposer_stats(rows)
    check('3 distinct proposers attributed', len(stats) == 3)

    # cross-log fallback: a second log that never started height 100's round 0
    # itself (e.g. parked/sync-following) must still get the proposer from log 1
    parked_fixture = FIXTURE.replace(
        '2026-06-27T03:15:28.634133Z  INFO 🔮 Started round height=100 round=0 role=None proposer=0x6791e002c8419e8a2fa5266e2951ed1780bc2295\n',
        '',
    )
    tmp2 = '/tmp/fleet-heights-selftest-validator2_cl.log'
    with open(tmp2, 'w') as fh:
        fh.write(parked_fixture)
    parsed2 = parse_log(tmp2)
    os.unlink(tmp2)
    check('validator2 fixture is missing height 100 round 0 locally', (100, 0) not in parsed2['started'])
    merged2, canonical2, _, _ = build_canonical({1: parsed, 2: parsed2})
    rows2 = attribute(canonical2, merged2)
    check(
        'height 100 still attributed via validator1 fallback, not UNKNOWN',
        next(r for r in rows2 if r['height'] == 100)['proposer'] != 'UNKNOWN',
    )

    print(f'\n{len(failures)} failure(s)' if failures else '\nall checks passed')
    return 1 if failures else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('run_dir', nargs='?', help='e.g. .quake/fleet-runs/v02-0914-2210')
    ap.add_argument('--json', action='store_true', help='machine-readable output')
    ap.add_argument('--selftest', action='store_true', help='run the built-in fixture tests and exit')
    args = ap.parse_args()

    if args.selftest:
        return selftest()
    if not args.run_dir:
        ap.error('run_dir is required unless --selftest is given')
    return run(args.run_dir, as_json=args.json)


if __name__ == '__main__':
    sys.exit(main())
