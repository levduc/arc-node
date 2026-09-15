// Copyright 2026 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Per-height phase decomposition, gated by `ARC_HEIGHT_TIMING=1`.
//!
//! One structured `INFO` line per consensus height, so an offline script
//! (`scripts/height-timing.py`) can attribute a height's wall clock to
//! proposal streaming, assembly, validation, voting, decide and anchor.
//!
//! **Off by default and free when off.** Every entry point begins with a
//! `LazyLock<bool>` read and returns before taking any clock reading or the
//! mutex, so a node without the variable pays one relaxed atomic load per
//! call site.
//!
//! # Shape
//!
//! Exactly one consensus instance runs per process, so the timer is a
//! process-global `Mutex<HeightTimer>` rather than a field of `State`. That
//! keeps the marks reachable from the places that actually observe the
//! phases — `payload.rs`, `finalize.rs`, the spawned stream task — without
//! threading a borrow of `State` through a dozen signatures that mock-based
//! tests also construct.
//!
//! The timer is reset by [`start_height`] on round 0 of each height, and the
//! *previous* height's line is emitted at that moment: by then every phase of
//! it, including the decide anchor and the forkchoice update, has happened.
//!
//! # Fidelity caveats (see the report)
//!
//! * `prevote` and `precommit` are the application's view of those votes:
//!   `prevote` is when the app hands consensus a complete, valid value (the
//!   input the prevote waits on) and `precommit` is the `ExtendVote` callback
//!   consensus makes immediately before sending its precommit. Neither is the
//!   moment the signed vote hits the wire.
//! * `lean_stage` is when the lean-lane structural validation finished and the
//!   speculative `arc_stageBlock` was dispatched; staging itself is
//!   deliberately fire-and-forget, so its completion is not on any path.

use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tracing::info;

use arc_consensus_types::Address;

/// A point in a height's life that gets a timestamp.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// First proposal part of this height's value arrived.
    FirstPart,
    /// Most recent proposal part arrived (overwritten by every part).
    LastPart,
    /// The parts formed a complete stream and decoded into a block.
    Assembled,
    /// Lean-lane structural validation done, speculative staging dispatched.
    LeanStage,
    /// `engine_newPayload` returned a verdict for the EVM lane.
    EvmNewPayload,
    /// A complete valid value was handed back to consensus (prevote input).
    Prevote,
    /// Consensus called `ExtendVote`, i.e. it is about to precommit.
    Precommit,
    /// The `Decided` message arrived from consensus.
    Decided,
    /// The lean lane's decide anchor (`arc_newBlock{commitment}`) returned.
    Anchor,
    /// `engine_forkchoiceUpdated` made the decided EVM block canonical.
    EvmFcu,
    /// Proposer only: the lean block came back from `arc_buildBlock`.
    BuildLean,
    /// Proposer only: the EVM payload came back from `engine_getPayload`.
    BuildEvm,
    /// Proposer only: all proposal parts were handed to the network.
    PartsSent,
}

/// A span of the decide path that gets a *duration*, not a timestamp.
///
/// The [`Phase`] marks tile a height with instants; these decompose the one
/// stretch where a pair of instants (`decided` -> `anchor` -> `evm_fcu`)
/// hides several distinct costs — store reads, store writes, the lean shim's
/// HTTP round trip, pruning — that a fleet run otherwise has to guess at.
///
/// Emitted as `dec_*` fields in milliseconds with one decimal: the whole
/// CL-side share of `decided -> anchor` measured 1-3 ms on a healthy
/// validator, which integer milliseconds would round away.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// `store_proposal_monitor_data`: a redb write transaction (fsync) for
    /// telemetry, taken before the decided block is even looked up.
    Monitor,
    /// `UndecidedBlocksRepository::get_by_hash`: find the decided value's row
    /// and SSZ-decode it.
    Lookup,
    /// `check_payload_binding` on the decided payload.
    Bind,
    /// `anchor_lean_lane`: the `arc_newBlock{commitment}` round trip(s),
    /// including any SYNCING re-polls.
    AnchorCall,
    /// The `ExecutionPayloadV3` clone handed to the decided-blocks store
    /// (O(value bytes)).
    PayloadClone,
    /// `DecidedBlocksRepository::store`: encode + redb write transaction.
    Store,
    /// `clean_stale_consensus_data`.
    Clean,
    /// `prune_historical_certs` + `prune_decided_blocks`.
    Prune,
    /// `prepare_next_height`: the two EL reads (signing validator set,
    /// consensus params) that gate the next `StartedRound`.
    NextHeight,
}

/// `true` when `ARC_HEIGHT_TIMING` is set to `1` / `true`. Read once.
static ENABLED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("ARC_HEIGHT_TIMING")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
});

/// Whether per-height timing lines are being emitted.
#[inline]
pub fn enabled() -> bool {
    *ENABLED
}

static TIMER: LazyLock<Mutex<HeightTimer>> = LazyLock::new(|| Mutex::new(HeightTimer::new()));

/// Lock the global timer, recovering from a poisoned mutex.
///
/// A panic elsewhere must not silently disable the instrumentation (and
/// `unwrap` is denied crate-wide); the timer holds no invariant that a partial
/// write could break.
fn timer() -> MutexGuard<'static, HeightTimer> {
    match TIMER.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Short form of a validator address for the log line: `0x` + 8 hex digits.
fn short_address(address: &Address) -> String {
    let full = address.to_string();
    match full.get(..10) {
        Some(prefix) => prefix.to_owned(),
        None => full,
    }
}

/// Begin timing `height`, emitting the previous height's line.
///
/// Called from round 0 of each height. `role` is the node's role in that round.
pub fn start_height(height: u64, role: &'static str, proposer: &Address) {
    if !enabled() {
        return;
    }
    let proposer = short_address(proposer);
    // Bind the line out of the `if let` scrutinee. Rust 2024's if-let
    // rescoping drops scrutinee temporaries before the `else` block only; in
    // the `then` block a `timer()` guard would live to the end of it, holding
    // the process-global mutex across the synchronous `info!`. Every other
    // tokio worker in `mark_at`/`record_part`/`record` would then block on a
    // stalled log sink — the instrumentation perturbing the path it measures.
    let line = timer().restart(height, role, proposer);
    if let Some(line) = line {
        info!("{line}");
    }
}

/// Record the round the node has reached within the current height.
pub fn set_round(round: i64) {
    if !enabled() {
        return;
    }
    timer().round = round;
}

/// Timestamp `phase` for whichever height is currently being timed.
///
/// Use [`mark_at`] wherever the caller knows the height and the mark could
/// race the height boundary.
pub fn mark(phase: Phase) {
    if !enabled() {
        return;
    }
    timer().mark(phase);
}

/// Timestamp `phase`, but only if `height` is the height being timed.
pub fn mark_at(height: u64, phase: Phase) {
    if !enabled() {
        return;
    }
    let mut t = timer();
    if t.height == height {
        t.mark(phase);
    }
}

/// Open a stopwatch for a [`Segment`], or `None` when timing is off.
///
/// Returning an `Option` rather than an `Instant` keeps the disabled path at
/// one relaxed atomic load: no clock is read, so a node without
/// `ARC_HEIGHT_TIMING` pays nothing for the call sites.
#[inline]
pub fn segment_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// Close a stopwatch opened by [`segment_start`] and add its elapsed time to
/// `segment` for `height`.
///
/// A no-op when `start` is `None` (timing off) or the timer has already moved
/// on to another height. Durations **accumulate**: a height that decides more
/// than once (a failed decide is retried) reports the total time it spent in
/// the segment, not just the first attempt's.
pub fn record(height: u64, segment: Segment, start: Option<Instant>) {
    let Some(start) = start else {
        return;
    };
    let elapsed = start.elapsed();
    let mut t = timer();
    if t.height == height {
        t.add_segment(segment, elapsed);
    }
}

/// Count one received proposal part of `bytes` bytes towards `height`.
pub fn record_part(height: u64, bytes: usize) {
    if !enabled() {
        return;
    }
    let mut t = timer();
    if t.height != height {
        return;
    }
    t.parts = t.parts.saturating_add(1);
    t.bytes = t
        .bytes
        .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    // `FirstPart` keeps the first arrival, `LastPart` advances with each.
    t.mark(Phase::FirstPart);
    t.mark(Phase::LastPart);
}

/// Record the size of the value at `height` (proposer path; receivers
/// accumulate it from the parts instead).
pub fn set_bytes(height: u64, bytes: u64) {
    if !enabled() {
        return;
    }
    let mut t = timer();
    if t.height == height {
        t.bytes = bytes;
    }
}

/// Record how many lean-lane transactions the height's value carries.
///
/// Unguarded: for the proposer's build path, which runs synchronously inside
/// the `GetValue` handler for the height being timed.
pub fn set_lean_txs(txs: u32) {
    if !enabled() {
        return;
    }
    timer().lean_txs = Some(txs);
}

/// Record the lean transaction count, but only for the height being timed.
///
/// The receive path also validates blocks for other heights (a pending
/// proposal re-offered at round start), which must not overwrite this
/// height's count.
pub fn set_lean_txs_at(height: u64, txs: u32) {
    if !enabled() {
        return;
    }
    let mut t = timer();
    if t.height == height {
        t.lean_txs = Some(txs);
    }
}

/// Milliseconds from `from` to `to`, saturating rather than panicking.
fn ms_between(from: Instant, to: Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_millis()).unwrap_or(u64::MAX)
}

/// The phase marks of the height currently in flight.
///
/// Every field is `Option`: a validator never builds, a proposer never
/// receives its own parts, and a height with the lean lane off never anchors.
/// Absent fields print as `-`.
pub struct HeightTimer {
    /// Whether a height has been started (so there is a line to emit).
    active: bool,
    height: u64,
    round: i64,
    role: &'static str,
    proposer: String,
    /// When this height started locally; the origin of every phase field.
    start: Instant,
    /// Milliseconds from the previous height's decide to this height's start.
    t_start_ms: Option<u64>,
    /// When the previous height decided; survives [`Self::restart`].
    prev_decided: Option<Instant>,
    parts: u32,
    bytes: u64,
    lean_txs: Option<u32>,

    first_part: Option<Instant>,
    last_part: Option<Instant>,
    assembled: Option<Instant>,
    lean_stage: Option<Instant>,
    evm_newpayload: Option<Instant>,
    prevote: Option<Instant>,
    precommit: Option<Instant>,
    decided: Option<Instant>,
    anchor: Option<Instant>,
    evm_fcu: Option<Instant>,
    build_lean: Option<Instant>,
    build_evm: Option<Instant>,
    parts_sent: Option<Instant>,

    /// Decide-path segment durations, in [`Segment`] order. `None` means the
    /// segment never ran on this node at this height.
    segments: [Option<Duration>; SEGMENTS],
}

/// How many [`Segment`] variants there are (kept next to [`SEGMENT_KEYS`],
/// which is what actually pins the order into the log line).
const SEGMENTS: usize = 9;

/// The `dec_*` field names, indexed by [`HeightTimer::segment_index`].
const SEGMENT_KEYS: [&str; SEGMENTS] = [
    "dec_monitor",
    "dec_lookup",
    "dec_bind",
    "dec_anchor_call",
    "dec_clone",
    "dec_store",
    "dec_clean",
    "dec_prune",
    "dec_next",
];

impl HeightTimer {
    /// An idle timer: no height started, so nothing to emit yet.
    pub fn new() -> Self {
        Self {
            active: false,
            height: 0,
            round: 0,
            role: "none",
            proposer: String::new(),
            start: Instant::now(),
            t_start_ms: None,
            prev_decided: None,
            parts: 0,
            bytes: 0,
            lean_txs: None,
            first_part: None,
            last_part: None,
            assembled: None,
            lean_stage: None,
            evm_newpayload: None,
            prevote: None,
            precommit: None,
            decided: None,
            anchor: None,
            evm_fcu: None,
            build_lean: None,
            build_evm: None,
            parts_sent: None,
            segments: [None; SEGMENTS],
        }
    }

    /// `segment`'s slot in [`Self::segments`] / [`SEGMENT_KEYS`].
    fn segment_index(segment: Segment) -> usize {
        match segment {
            Segment::Monitor => 0,
            Segment::Lookup => 1,
            Segment::Bind => 2,
            Segment::AnchorCall => 3,
            Segment::PayloadClone => 4,
            Segment::Store => 5,
            Segment::Clean => 6,
            Segment::Prune => 7,
            Segment::NextHeight => 8,
        }
    }

    /// Add `elapsed` to `segment`'s running total for this height.
    pub fn add_segment(&mut self, segment: Segment, elapsed: Duration) {
        if !self.active {
            return;
        }
        let slot = &mut self.segments[Self::segment_index(segment)];
        *slot = Some(match *slot {
            Some(total) => total.saturating_add(elapsed),
            None => elapsed,
        });
    }

    /// The slot holding `phase`'s timestamp.
    fn slot(&mut self, phase: Phase) -> &mut Option<Instant> {
        match phase {
            Phase::FirstPart => &mut self.first_part,
            Phase::LastPart => &mut self.last_part,
            Phase::Assembled => &mut self.assembled,
            Phase::LeanStage => &mut self.lean_stage,
            Phase::EvmNewPayload => &mut self.evm_newpayload,
            Phase::Prevote => &mut self.prevote,
            Phase::Precommit => &mut self.precommit,
            Phase::Decided => &mut self.decided,
            Phase::Anchor => &mut self.anchor,
            Phase::EvmFcu => &mut self.evm_fcu,
            Phase::BuildLean => &mut self.build_lean,
            Phase::BuildEvm => &mut self.build_evm,
            Phase::PartsSent => &mut self.parts_sent,
        }
    }

    /// Stamp `phase` with the current instant.
    ///
    /// First occurrence wins for every phase except [`Phase::LastPart`], which
    /// is meant to advance with each part. A round change re-runs validation
    /// and voting, and the first pass is the one that describes round 0.
    pub fn mark(&mut self, phase: Phase) {
        if !self.active {
            return;
        }
        let now = Instant::now();
        if phase == Phase::Decided {
            self.prev_decided = Some(now);
        }
        let slot = self.slot(phase);
        if phase == Phase::LastPart || slot.is_none() {
            *slot = Some(now);
        }
    }

    /// Start timing `height`, returning the finished line for the height that
    /// was in flight (if any).
    pub fn restart(&mut self, height: u64, role: &'static str, proposer: String) -> Option<String> {
        let line = self.active.then(|| self.format_line());
        let now = Instant::now();
        let prev_decided = self.prev_decided;

        *self = Self::new();
        self.active = true;
        self.height = height;
        self.role = role;
        self.proposer = proposer;
        self.start = now;
        self.prev_decided = prev_decided;
        self.t_start_ms = prev_decided.map(|d| ms_between(d, now));

        line
    }

    /// `phase`'s timestamp in milliseconds after this height's start, or `-`.
    fn rel(&self, at: Option<Instant>) -> String {
        match at {
            Some(t) => ms_between(self.start, t).to_string(),
            None => "-".to_owned(),
        }
    }

    /// The single structured line for this height.
    ///
    /// Every field is always present; `-` means the phase did not happen on
    /// this node. The parser in `scripts/height-timing.py` depends on this
    /// exact `key=value` vocabulary.
    pub fn format_line(&self) -> String {
        let dash = |v: Option<u64>| match v {
            Some(v) => v.to_string(),
            None => "-".to_owned(),
        };

        format!(
            "height_timing h={h} role={role} proposer={proposer} round={round} \
             t_start={t_start} first_part={first_part} last_part={last_part} \
             parts={parts} bytes={bytes} assembled={assembled} lean_stage={lean_stage} \
             evm_newpayload={evm_newpayload} prevote={prevote} precommit={precommit} \
             decided={decided} anchor={anchor} evm_fcu={evm_fcu} lean_txs={lean_txs} \
             build_lean={build_lean} build_evm={build_evm} parts_sent={parts_sent}{segments}",
            h = self.height,
            role = self.role,
            proposer = self.proposer,
            round = self.round,
            t_start = dash(self.t_start_ms),
            first_part = self.rel(self.first_part),
            last_part = self.rel(self.last_part),
            parts = self.parts,
            bytes = self.bytes,
            assembled = self.rel(self.assembled),
            lean_stage = self.rel(self.lean_stage),
            evm_newpayload = self.rel(self.evm_newpayload),
            prevote = self.rel(self.prevote),
            precommit = self.rel(self.precommit),
            decided = self.rel(self.decided),
            anchor = self.rel(self.anchor),
            evm_fcu = self.rel(self.evm_fcu),
            lean_txs = dash(self.lean_txs.map(u64::from)),
            build_lean = self.rel(self.build_lean),
            build_evm = self.rel(self.build_evm),
            parts_sent = self.rel(self.parts_sent),
            segments = self.segment_fields(),
        )
    }

    /// The `dec_*` decide-path duration fields, in [`SEGMENT_KEYS`] order,
    /// each prefixed with a space so they append to the line as-is.
    ///
    /// Milliseconds with one decimal; `-` for a segment that did not run.
    fn segment_fields(&self) -> String {
        let mut out = String::new();
        for (key, slot) in SEGMENT_KEYS.iter().zip(self.segments.iter()) {
            match slot {
                Some(d) => out.push_str(&format!(" {key}={:.1}", d.as_secs_f64() * 1000.0)),
                None => out.push_str(&format!(" {key}=-")),
            }
        }
        out
    }
}

impl Default for HeightTimer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    /// A freshly started timer, so phases can be stamped at known offsets
    /// from its start without sleeping.
    fn timer_at(height: u64, role: &'static str) -> HeightTimer {
        let mut t = HeightTimer::new();
        let _ = t.restart(height, role, "0x12345678".to_owned());
        t
    }

    /// Stamp `phase` as if it happened `offset` after the height started.
    fn stamp(t: &mut HeightTimer, phase: Phase, offset: Duration) {
        let at = t.start.checked_add(offset).expect("offset fits an Instant");
        *t.slot(phase) = Some(at);
    }

    fn field<'a>(line: &'a str, key: &str) -> &'a str {
        line.split_whitespace()
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("line has no field {key}: {line}"))
    }

    /// `start_height` must not hold the global timer mutex while it logs.
    ///
    /// This mirrors its body: the guard is a temporary of a `let`, so it is
    /// dropped at the end of that statement — before the point where `info!`
    /// runs. With the guard as an `if let` scrutinee instead it would still be
    /// alive here (edition 2024 rescopes scrutinee temporaries out of the
    /// `else` block only, not the `then` block). The line must come out
    /// byte-identical either way.
    #[test]
    fn start_height_releases_the_timer_before_logging_the_line() {
        // Prime the global timer: the first restart has nothing to emit.
        assert!(timer()
            .restart(900, "validator", "0x12345678".to_owned())
            .is_none());
        // What the in-flight height must format to, taken under its own lock.
        let expected = timer().format_line();

        let line = timer().restart(901, "proposer", "0xaabbccdd".to_owned());
        // The `info!` in `start_height` executes at exactly this point.
        assert!(
            TIMER.try_lock().is_ok(),
            "the global timer is still locked where the line is logged"
        );
        assert_eq!(line.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn a_fresh_timer_emits_nothing() {
        let mut t = HeightTimer::new();
        assert!(t.restart(7, "validator", "0xaabbccdd".to_owned()).is_none());
    }

    #[test]
    fn unmarked_phases_print_as_dash() {
        let t = timer_at(42, "validator");
        let line = t.format_line();

        assert!(line.starts_with("height_timing h=42 "), "{line}");
        assert_eq!(field(&line, "role"), "validator");
        assert_eq!(field(&line, "proposer"), "0x12345678");
        assert_eq!(field(&line, "round"), "0");
        // No previous decide is known yet, and no phase has happened.
        assert_eq!(field(&line, "t_start"), "-");
        for key in [
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
            "lean_txs",
        ] {
            assert_eq!(field(&line, key), "-", "expected {key} to be absent");
        }
        // Counters are always numeric, never `-`.
        assert_eq!(field(&line, "parts"), "0");
        assert_eq!(field(&line, "bytes"), "0");
    }

    #[test]
    fn phases_print_as_milliseconds_after_the_height_start() {
        let mut t = timer_at(100, "validator");
        stamp(&mut t, Phase::FirstPart, Duration::from_millis(35));
        stamp(&mut t, Phase::LastPart, Duration::from_millis(180));
        stamp(&mut t, Phase::Assembled, Duration::from_millis(181));
        stamp(&mut t, Phase::EvmNewPayload, Duration::from_millis(250));
        stamp(&mut t, Phase::LeanStage, Duration::from_millis(262));
        stamp(&mut t, Phase::Prevote, Duration::from_millis(263));
        stamp(&mut t, Phase::Precommit, Duration::from_millis(310));
        stamp(&mut t, Phase::Decided, Duration::from_millis(402));
        stamp(&mut t, Phase::Anchor, Duration::from_millis(418));
        stamp(&mut t, Phase::EvmFcu, Duration::from_millis(455));
        t.parts = 14;
        t.bytes = 1_234_567;
        t.lean_txs = Some(2400);
        t.round = 1;

        let line = t.format_line();

        assert_eq!(field(&line, "round"), "1");
        assert_eq!(field(&line, "first_part"), "35");
        assert_eq!(field(&line, "last_part"), "180");
        assert_eq!(field(&line, "assembled"), "181");
        assert_eq!(field(&line, "evm_newpayload"), "250");
        assert_eq!(field(&line, "lean_stage"), "262");
        assert_eq!(field(&line, "prevote"), "263");
        assert_eq!(field(&line, "precommit"), "310");
        assert_eq!(field(&line, "decided"), "402");
        assert_eq!(field(&line, "anchor"), "418");
        assert_eq!(field(&line, "evm_fcu"), "455");
        assert_eq!(field(&line, "parts"), "14");
        assert_eq!(field(&line, "bytes"), "1234567");
        assert_eq!(field(&line, "lean_txs"), "2400");
        // A validator builds nothing.
        assert_eq!(field(&line, "build_lean"), "-");
        assert_eq!(field(&line, "parts_sent"), "-");
    }

    #[test]
    fn the_proposer_line_carries_the_build_fields() {
        let mut t = timer_at(101, "proposer");
        stamp(&mut t, Phase::BuildLean, Duration::from_millis(12));
        stamp(&mut t, Phase::BuildEvm, Duration::from_millis(48));
        stamp(&mut t, Phase::PartsSent, Duration::from_millis(96));

        let line = t.format_line();

        assert_eq!(field(&line, "role"), "proposer");
        assert_eq!(field(&line, "build_lean"), "12");
        assert_eq!(field(&line, "build_evm"), "48");
        assert_eq!(field(&line, "parts_sent"), "96");
        // The proposer never receives its own parts.
        assert_eq!(field(&line, "first_part"), "-");
    }

    #[test]
    fn restart_emits_the_previous_height_and_carries_the_decide_gap() {
        let mut t = timer_at(200, "validator");
        t.mark(Phase::Decided);

        let line = t
            .restart(201, "proposer", "0xdeadbeef".to_owned())
            .expect("the finished height must be emitted");
        assert!(line.starts_with("height_timing h=200 "), "{line}");

        // The new height measures its start against the previous decide, which
        // just happened, so the gap is small but present (never `-`).
        let next = t.format_line();
        assert!(next.starts_with("height_timing h=201 "), "{next}");
        assert_ne!(field(&next, "t_start"), "-");
        assert!(
            field(&next, "t_start").parse::<u64>().unwrap_or(u64::MAX) < 1_000,
            "{next}"
        );
    }

    #[test]
    fn first_occurrence_wins_except_for_the_last_part() {
        let mut t = timer_at(300, "validator");

        t.mark(Phase::Prevote);
        let first = t.prevote;
        t.mark(Phase::Prevote);
        assert_eq!(t.prevote, first, "a re-offered value must not move prevote");

        t.mark(Phase::FirstPart);
        let pinned = t.first_part;
        t.mark(Phase::LastPart);
        let early = t.last_part;
        t.mark(Phase::FirstPart);
        t.mark(Phase::LastPart);
        assert_eq!(
            t.first_part, pinned,
            "first_part must keep the first arrival"
        );
        assert_ne!(t.last_part, early, "last_part must follow the newest part");
    }

    #[test]
    fn an_idle_timer_ignores_marks() {
        let mut t = HeightTimer::new();
        t.mark(Phase::Prevote);
        assert!(t.prevote.is_none());
    }

    #[test]
    fn the_line_is_a_single_line_of_key_value_pairs() {
        let line = timer_at(1, "validator").format_line();
        assert!(!line.contains('\n'), "{line}");
        let mut fields = line.split_whitespace();
        assert_eq!(fields.next(), Some("height_timing"));
        for kv in fields {
            assert!(kv.contains('='), "field {kv} is not key=value: {line}");
        }
    }

    // ---- decide-path segments (`dec_*`) ---------------------------------

    #[test]
    fn decide_segments_print_as_dash_until_they_run() {
        let line = timer_at(7, "validator").format_line();
        for key in SEGMENT_KEYS {
            assert_eq!(field(&line, key), "-", "expected {key} to be absent");
        }
    }

    #[test]
    fn decide_segments_print_milliseconds_with_one_decimal() {
        let mut t = timer_at(8, "validator");
        t.add_segment(Segment::Monitor, Duration::from_micros(1_460));
        t.add_segment(Segment::Lookup, Duration::from_micros(420));
        t.add_segment(Segment::AnchorCall, Duration::from_millis(203));
        t.add_segment(Segment::NextHeight, Duration::from_micros(50));

        let line = t.format_line();

        assert_eq!(field(&line, "dec_monitor"), "1.5");
        assert_eq!(field(&line, "dec_lookup"), "0.4");
        assert_eq!(field(&line, "dec_anchor_call"), "203.0");
        // Sub-100us still shows as a number, not `-`: the point of the decimal
        // is that the CL-side share of decided->anchor is single-digit ms.
        assert_eq!(field(&line, "dec_next"), "0.1");
        // Segments that did not run stay absent.
        assert_eq!(field(&line, "dec_store"), "-");
        assert_eq!(field(&line, "dec_prune"), "-");
    }

    #[test]
    fn decide_segments_accumulate_across_repeated_decides() {
        let mut t = timer_at(9, "validator");
        t.add_segment(Segment::Store, Duration::from_millis(10));
        t.add_segment(Segment::Store, Duration::from_millis(5));

        assert_eq!(field(&t.format_line(), "dec_store"), "15.0");
    }

    #[test]
    fn an_idle_timer_ignores_segments() {
        let mut t = HeightTimer::new();
        t.add_segment(Segment::AnchorCall, Duration::from_millis(100));
        assert!(t.segments.iter().all(Option::is_none));
    }

    #[test]
    fn restarting_clears_the_previous_heights_segments() {
        let mut t = timer_at(10, "validator");
        t.add_segment(Segment::Clean, Duration::from_millis(3));
        let line = t
            .restart(11, "validator", "0x12345678".to_owned())
            .expect("the finished height must be emitted");

        assert_eq!(field(&line, "dec_clean"), "3.0");
        assert_eq!(field(&t.format_line(), "dec_clean"), "-");
    }

    /// The field order in the line must match `SEGMENT_KEYS`, since that is
    /// the vocabulary `scripts/height-timing.py` parses.
    #[test]
    fn every_segment_has_exactly_one_field_in_the_line() {
        let line = timer_at(12, "validator").format_line();
        for key in SEGMENT_KEYS {
            assert_eq!(
                line.matches(&format!("{key}=")).count(),
                1,
                "{key} must appear exactly once: {line}"
            );
        }
        assert_eq!(SEGMENT_KEYS.len(), SEGMENTS);
    }

    #[test]
    fn short_address_keeps_the_prefix() {
        let address = Address::new([0xAB; 20]);
        assert_eq!(short_address(&address), "0xabababab");
    }
}
