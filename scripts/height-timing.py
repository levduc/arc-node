#!/usr/bin/env python3
"""Aggregate the CL's per-height `height_timing` lines into a phase decomposition.

The consensus layer emits one INFO line per height when `ARC_HEIGHT_TIMING=1`:

    height_timing h=42 role=validator proposer=0x1234abcd round=0 t_start=12
      first_part=35 last_part=180 parts=14 bytes=1234567 assembled=181
      lean_stage=262 evm_newpayload=250 prevote=263 precommit=310 decided=402
      anchor=418 evm_fcu=455 lean_txs=2400 build_lean=- build_evm=- parts_sent=-
      dec_monitor=0.8 dec_lookup=2.3 dec_bind=0.0 dec_anchor_call=11.9
      dec_clone=0.4 dec_store=3.1 dec_clean=0.9 dec_prune=0.2 dec_next=1.5

Every phase field is milliseconds after the *start of that height*; `t_start` is
milliseconds from the previous height's decide to this height's start; `-` means
the phase did not happen on this node. This script turns those absolute stamps
into per-phase durations and reports mean / p50 / p90 for each, split by role
and by proposer, plus the cadence per wall-clock minute.

The `dec_*` fields are the exception: they are elapsed DURATIONS (ms, one
decimal) for the decide path's individual costs, so `decided -> anchor` can be
split into the CL's own work and the lean shim's round trip. Older logs have
no `dec_*` fields and parse exactly as before.

Usage:  scripts/height-timing.py <cl.log> [<cl.log> ...]
        scripts/height-timing.py --self-test
"""

from __future__ import annotations

import argparse
import io
import math
import re
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from typing import Iterable, Iterator, Optional, Sequence

LINE_RE = re.compile(r"height_timing\s+h=\d+.*$")
KV_RE = re.compile(r"([a-z_]+)=(\S+)")

# Timestamp fields, in the order the CL writes them.
STAMP_FIELDS = (
    "t_start",
    "first_part",
    "last_part",
    "assembled",
    "lean_stage",
    "evm_newpayload",
    "prevote",
    "precommit",
    "decided",
    "anchor",
    "evm_fcu",
    "build_lean",
    "build_evm",
    "parts_sent",
)
COUNT_FIELDS = ("round", "parts", "bytes", "lean_txs")

# Decide-path segment DURATIONS (ms, one decimal) — unlike every other field
# these are elapsed times, not stamps relative to the height start. They
# decompose `decided -> anchor -> evm_fcu` and the gap to the next height.
SEGMENT_FIELDS = (
    "dec_monitor",
    "dec_lookup",
    "dec_bind",
    "dec_anchor_call",
    "dec_clone",
    "dec_store",
    "dec_clean",
    "dec_prune",
    "dec_next",
)

# name -> (from_field, to_field). `None` as the source means "the height start",
# which is the origin every stamp is already relative to.
PHASES: tuple[tuple[str, Optional[str], str], ...] = (
    ("start_to_first_part", None, "first_part"),
    ("first_to_last_part", "first_part", "last_part"),
    ("last_part_to_assembled", "last_part", "assembled"),
    ("assembled_to_lean_stage", "assembled", "lean_stage"),
    ("assembled_to_newpayload", "assembled", "evm_newpayload"),
    ("validate_to_prevote", "lean_stage", "prevote"),
    ("prevote_to_precommit", "prevote", "precommit"),
    ("precommit_to_decided", "precommit", "decided"),
    ("decided_to_anchor", "decided", "anchor"),
    ("anchor_to_fcu", "anchor", "evm_fcu"),
    # Proposer-side build.
    ("start_to_build_lean", None, "build_lean"),
    ("build_lean_to_build_evm", "build_lean", "build_evm"),
    ("build_evm_to_parts_sent", "build_evm", "parts_sent"),
    # Whole-height spans.
    ("start_to_decided", None, "decided"),
    ("gap_decide_to_next_start", None, "t_start"),
)


@dataclass
class Height:
    """One parsed `height_timing` line."""

    h: int
    role: str
    proposer: str
    stamps: dict[str, int] = field(default_factory=dict)
    counts: dict[str, int] = field(default_factory=dict)
    segments: dict[str, float] = field(default_factory=dict)
    source: str = ""

    def duration(self, frm: Optional[str], to: str) -> Optional[int]:
        """Milliseconds between two stamps, or None if either is missing."""
        end = self.stamps.get(to)
        if end is None:
            return None
        if frm is None:
            return end
        start = self.stamps.get(frm)
        if start is None:
            return None
        return end - start


def parse_line(line: str, source: str = "") -> Optional[Height]:
    """Parse one log line, or return None if it is not a height_timing line."""
    match = LINE_RE.search(line)
    if not match:
        return None
    kvs = dict(KV_RE.findall(match.group(0)))
    raw_h = kvs.get("h")
    if raw_h is None or not raw_h.isdigit():
        return None

    rec = Height(
        h=int(raw_h),
        role=kvs.get("role", "unknown"),
        proposer=kvs.get("proposer", "unknown"),
        source=source,
    )
    for key in STAMP_FIELDS:
        raw = kvs.get(key)
        if raw is None or raw == "-":
            continue
        try:
            rec.stamps[key] = int(raw)
        except ValueError:
            continue
    for key in COUNT_FIELDS:
        raw = kvs.get(key)
        if raw is None or raw == "-":
            continue
        try:
            rec.counts[key] = int(raw)
        except ValueError:
            continue
    for key in SEGMENT_FIELDS:
        raw = kvs.get(key)
        if raw is None or raw == "-":
            continue
        try:
            rec.segments[key] = float(raw)
        except ValueError:
            continue
    return rec


def parse_lines(lines: Iterable[str], source: str = "") -> Iterator[Height]:
    for line in lines:
        rec = parse_line(line, source)
        if rec is not None:
            yield rec


def percentile(values: Sequence[float], q: float) -> float:
    """Nearest-rank percentile of a non-empty sample; q in [0, 1].

    Nearest rank (ceil), not interpolation: with a handful of heights an
    interpolated p90 invents a value no height actually took.
    """
    ordered = sorted(values)
    if not ordered:
        raise ValueError("percentile of an empty sample")
    rank = max(1, min(len(ordered), math.ceil(q * len(ordered))))
    return ordered[rank - 1]


def summarize(values: Sequence[float]) -> Optional[dict[str, float]]:
    if not values:
        return None
    return {
        "n": len(values),
        "mean": sum(values) / len(values),
        "p50": percentile(values, 0.50),
        "p90": percentile(values, 0.90),
    }


def phase_table(records: Sequence[Height]) -> list[tuple[str, dict[str, float]]]:
    """mean/p50/p90 for every phase that at least one record observed."""
    out = []
    for name, frm, to in PHASES:
        samples = []
        for rec in records:
            value = rec.duration(frm, to)
            if value is not None:
                samples.append(float(value))
        stats = summarize(samples)
        if stats is not None:
            out.append((name, stats))
    return out


def render_phases(title: str, records: Sequence[Height], out) -> None:
    rows = phase_table(records)
    print(f"\n{title}  (heights={len(records)})", file=out)
    if not rows:
        print("  no phase observed", file=out)
        return
    print(f"  {'phase':<28} {'n':>5} {'mean':>9} {'p50':>9} {'p90':>9}", file=out)
    for name, s in rows:
        print(
            f"  {name:<28} {int(s['n']):>5} {s['mean']:>9.1f} "
            f"{s['p50']:>9.1f} {s['p90']:>9.1f}",
            file=out,
        )


def render_segments(title: str, records: Sequence[Height], out) -> None:
    """The `dec_*` decide-path breakdown, for records that carry it.

    Prints nothing when no height has any segment — a log from a CL without
    the instrumentation reads exactly as it did before.
    """
    rows = []
    for key in SEGMENT_FIELDS:
        samples = [r.segments[key] for r in records if key in r.segments]
        stats = summarize(samples)
        if stats is not None:
            rows.append((key, stats))
    if not rows:
        return
    print(f"\n{title}  (heights={len(records)})", file=out)
    print(f"  {'segment':<28} {'n':>5} {'mean':>9} {'p50':>9} {'p90':>9}", file=out)
    for name, s in rows:
        print(
            f"  {name:<28} {int(s['n']):>5} {s['mean']:>9.2f} "
            f"{s['p50']:>9.2f} {s['p90']:>9.2f}",
            file=out,
        )
    # The budget these fields exist to explain: everything the CL does between
    # the certificate arriving and the anchor returning. The residual against
    # `decided_to_anchor` is what is still unattributed.
    pre = ("dec_monitor", "dec_lookup", "dec_bind", "dec_anchor_call")
    accounted = [
        sum(r.segments[k] for k in pre if k in r.segments)
        for r in records
        if any(k in r.segments for k in pre)
    ]
    measured = [
        float(v) for v in (r.duration("decided", "anchor") for r in records) if v is not None
    ]
    a, m = summarize(accounted), summarize(measured)
    if a and m:
        print(
            f"  {'-> sum(pre-anchor dec_*)':<28} {int(a['n']):>5} {a['mean']:>9.2f} "
            f"{a['p50']:>9.2f} {a['p90']:>9.2f}",
            file=out,
        )
        print(
            f"  {'-> decided_to_anchor':<28} {int(m['n']):>5} {m['mean']:>9.2f} "
            f"{m['p50']:>9.2f} {m['p90']:>9.2f}",
            file=out,
        )


def render_sizes(records: Sequence[Height], out) -> None:
    for key in ("bytes", "parts", "lean_txs"):
        samples = [float(r.counts[key]) for r in records if key in r.counts]
        stats = summarize(samples)
        if stats is None:
            continue
        print(
            f"  {key:<10} n={int(stats['n']):<6} mean={stats['mean']:>12.1f} "
            f"p50={stats['p50']:>12.1f} p90={stats['p90']:>12.1f}",
            file=out,
        )


def cadence(records: Sequence[Height], out) -> None:
    """Heights per minute, from the height-to-height wall clock.

    Each height's wall time is its own `start_to_decided` plus the gap from the
    previous decide to its start (`t_start`), which together tile the timeline
    with no overlap. Heights missing either field are skipped.
    """
    spans = []
    for rec in records:
        decided = rec.stamps.get("decided")
        gap = rec.stamps.get("t_start")
        if decided is None or gap is None:
            continue
        spans.append(decided + gap)
    if not spans:
        print("\ncadence: not enough complete heights", file=out)
        return
    total_ms = sum(spans)
    mean_ms = total_ms / len(spans)
    print(f"\ncadence  (heights={len(spans)}, wall={total_ms / 1000.0:.1f}s)", file=out)
    print(
        f"  mean height {mean_ms:.1f} ms  ->  "
        f"{60000.0 / mean_ms:.2f} heights/min  ({1000.0 / mean_ms:.2f} blk/s)",
        file=out,
    )
    print(
        f"  p50 height {percentile(spans, 0.50):.1f} ms   "
        f"p90 height {percentile(spans, 0.90):.1f} ms",
        file=out,
    )


def report(records: Sequence[Height], out=sys.stdout) -> None:
    print(f"height_timing: {len(records)} heights parsed", file=out)
    if not records:
        return
    heights = [r.h for r in records]
    print(f"  height range {min(heights)}..{max(heights)}", file=out)
    print("  value size:", file=out)
    render_sizes(records, out)

    cadence(records, out)

    render_phases("ALL", records, out)
    render_segments("ALL decide-path segments", records, out)

    by_role: dict[str, list[Height]] = defaultdict(list)
    for rec in records:
        by_role[rec.role].append(rec)
    for role in sorted(by_role):
        render_phases(f"role={role}", by_role[role], out)
        render_segments(f"role={role} decide-path segments", by_role[role], out)

    by_proposer: dict[str, list[Height]] = defaultdict(list)
    for rec in records:
        by_proposer[rec.proposer].append(rec)
    for proposer in sorted(by_proposer):
        render_phases(f"proposer={proposer}", by_proposer[proposer], out)


def read_files(paths: Sequence[str]) -> list[Height]:
    records: list[Height] = []
    for path in paths:
        with open(path, "r", errors="replace") as handle:
            records.extend(parse_lines(handle, source=path))
    records.sort(key=lambda r: (r.source, r.h))
    return records


# --------------------------------------------------------------------------
# Self-test: a fixture of hand-written lines, so the parser and the statistics
# are checked without a fleet run.
# --------------------------------------------------------------------------

FIXTURE = """\
2026-09-14T20:00:00.000Z  INFO arc_node_consensus::app: unrelated line
2026-09-14T20:00:00.100Z  INFO height_timing h=100 role=validator proposer=0xaaaa1111 round=0 t_start=10 first_part=30 last_part=180 parts=14 bytes=1200000 assembled=185 lean_stage=260 evm_newpayload=250 prevote=262 precommit=300 decided=400 anchor=415 evm_fcu=450 lean_txs=2400 build_lean=- build_evm=- parts_sent=- dec_monitor=0.8 dec_lookup=2.3 dec_bind=0.0 dec_anchor_call=11.9 dec_clone=0.4 dec_store=3.1 dec_clean=0.9 dec_prune=0.2 dec_next=1.5
2026-09-14T20:00:00.600Z  INFO height_timing h=101 role=proposer proposer=0xbbbb2222 round=0 t_start=20 first_part=- last_part=- parts=0 bytes=1300000 assembled=- lean_stage=- evm_newpayload=90 prevote=95 precommit=200 decided=300 anchor=320 evm_fcu=350 lean_txs=2500 build_lean=30 build_evm=80 parts_sent=140
2026-09-14T20:00:01.100Z  INFO height_timing h=102 role=validator proposer=0xcccc3333 round=1 t_start=30 first_part=50 last_part=400 parts=20 bytes=1400000 assembled=410 lean_stage=500 evm_newpayload=480 prevote=505 precommit=600 decided=800 anchor=830 evm_fcu=880 lean_txs=2600 build_lean=- build_evm=- parts_sent=- dec_monitor=1.2 dec_lookup=2.7 dec_bind=- dec_anchor_call=25.1 dec_clone=0.6 dec_store=4.0 dec_clean=1.0 dec_prune=0.3 dec_next=2.0
"""


def self_test() -> int:
    records = list(parse_lines(FIXTURE.splitlines()))
    failures: list[str] = []

    def check(label: str, got, want) -> None:
        if got != want:
            failures.append(f"{label}: got {got!r}, want {want!r}")

    check("parsed count", len(records), 3)
    check("heights", [r.h for r in records], [100, 101, 102])
    check("roles", [r.role for r in records], ["validator", "proposer", "validator"])
    check("proposers", records[1].proposer, "0xbbbb2222")
    check("round parsed", records[2].counts.get("round"), 1)
    check("bytes parsed", records[0].counts.get("bytes"), 1200000)
    check("lean_txs parsed", records[1].counts.get("lean_txs"), 2500)

    # `-` fields must be absent, not zero.
    check("dash is absent", "first_part" in records[1].stamps, False)
    check("dash duration is None", records[1].duration("first_part", "last_part"), None)

    # Durations.
    check("stream span h100", records[0].duration("first_part", "last_part"), 150)
    check("assemble h100", records[0].duration("last_part", "assembled"), 5)
    check("vote gap h100", records[0].duration("prevote", "precommit"), 38)
    check("commit h100", records[0].duration("precommit", "decided"), 100)
    check("anchor h100", records[0].duration("decided", "anchor"), 15)
    check("start-to-first h100", records[0].duration(None, "first_part"), 30)
    check("build h101", records[1].duration("build_lean", "build_evm"), 50)

    # Aggregation: the two validator heights only.
    validators = [r for r in records if r.role == "validator"]
    rows = dict(phase_table(validators))
    check("validator n", rows["first_to_last_part"]["n"], 2)
    check("validator mean", rows["first_to_last_part"]["mean"], 250.0)
    check("validator p50", rows["first_to_last_part"]["p50"], 150.0)
    check("validator p90", rows["first_to_last_part"]["p90"], 350.0)

    # The proposer's build phases must not appear in the validator table.
    check("no build for validators", "build_lean_to_build_evm" in rows, False)

    # Percentiles on a known sample.
    check("p50 of 1..10", percentile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.5), 5)
    check("p90 of 1..10", percentile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.9), 9)
    check("p50 single", percentile([7], 0.5), 7)

    # Cadence tiles t_start + decided: 410, 320, 830 ms.
    buf = io.StringIO()
    cadence(records, buf)
    text = buf.getvalue()
    if "heights=3" not in text:
        failures.append(f"cadence did not use all 3 heights: {text!r}")
    if "520.0 ms" not in text:
        failures.append(f"cadence mean should be 520.0 ms: {text!r}")

    # A full report must not raise on the fixture.
    buf = io.StringIO()
    report(records, buf)
    for expected in ("role=validator", "role=proposer", "proposer=0xaaaa1111", "cadence"):
        if expected not in buf.getvalue():
            failures.append(f"report is missing section {expected!r}")

    # Decide-path segments: floats, `-` absent, and a line written by a CL
    # without the instrumentation (h=101) parses exactly as before.
    check("segment parsed", records[0].segments.get("dec_anchor_call"), 11.9)
    check("segment zero kept", records[0].segments.get("dec_bind"), 0.0)
    check("segment dash absent", "dec_bind" in records[2].segments, False)
    check("old-format line has no segments", records[1].segments, {})

    buf = io.StringIO()
    render_segments("seg", records, buf)
    text = buf.getvalue()
    for expected in ("dec_anchor_call", "sum(pre-anchor dec_*)", "decided_to_anchor"):
        if expected not in text:
            failures.append(f"segment table is missing {expected!r}: {text!r}")
    # h=100: 0.8 + 2.3 + 0.0 + 11.9 = 15.0; h=102: 1.2 + 2.7 + 25.1 = 29.0.
    if "15.00" not in text or "29.00" not in text:
        failures.append(f"segment sums wrong: {text!r}")

    # A CL log with no segments at all must print no segment table.
    buf = io.StringIO()
    render_segments("seg", [records[1]], buf)
    if buf.getvalue() != "":
        failures.append(f"segment table should be empty: {buf.getvalue()!r}")

    # Non-matching input yields nothing.
    check("noise ignored", parse_line("2026-01-01 INFO nothing here"), None)

    if failures:
        for f in failures:
            print(f"FAIL {f}", file=sys.stderr)
        print(f"{len(failures)} failure(s)", file=sys.stderr)
        return 1
    print("height-timing.py self-test: all checks passed")
    return 0


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="*", help="CL log files containing height_timing lines")
    parser.add_argument("--self-test", action="store_true", help="run the built-in unit tests")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()
    if not args.logs:
        parser.print_usage(sys.stderr)
        return 2

    records = read_files(args.logs)
    if not records:
        print(
            "no height_timing lines found — was the CL run with ARC_HEIGHT_TIMING=1?",
            file=sys.stderr,
        )
        return 1
    report(records)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
