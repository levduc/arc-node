# MISSION 2: optimise the payment-lane EL (CL/consensus path is OFF LIMITS)

**Hard constraint from the user: do NOT touch the CL / consensus path.** No changes to proposal
streaming, SSZ framing, voting, or `crates/malachite-app`. Everything must land in the EL —
`crates/evm`, `crates/execution-*`, `crates/evm-node`, or EL launch flags.

## Why this mission exists (and the mistake that motivated it)

Mission 1 shipped a native-transfer fast path (exec 11.03 -> 7.00 us/tx, 1.58x) and then measured
"EL = 10% of block time, CL = 90%". **That 10/90 split is WRONG in the EL's favour**: it was computed
as `block_time - newPayload_elapsed`, but `newPayload_elapsed` is reth's INTERNAL timer, which starts
*after* the engine-API request is deserialised.

The payment EL is driven over **authrpc HTTP JSON-RPC** (`--authrpc.port=8551`, see
`launch-payment-els.sh`), while the EVM lane uses **IPC**. So every `engine_newPayloadV3` for the
payment lane arrives as JSON with all 47,618 transactions as hex strings — roughly **12 MB of JSON
per block** to parse, hex-decode and re-encode into reth types. That work is **inside the EL** but
**outside** the 357 ms I attributed to it. It was filed under "consensus overhead". It is not.

## Definition of done

1. The EL-side cost of ingesting a full 1-Ggas payload is **measured and attributed** (not inferred).
2. At least one EL-only change lands that measurably cuts payment-lane block cost, verified by the
   usual gates.
3. All 4 validators still agree on the payment-lane state root for 100+ consecutive blocks under load.
4. Nothing regressed: EVM lane stock, block production healthy.

## Ranked hypotheses (measure before optimising — this is the whole point)

1. **Engine-API ingestion (UNMEASURED, top suspect).** Gap between the CL issuing `newPayload` and
   reth's internal timer starting. Fixes are EL-side: switch the payment lane to **IPC** (the EVM
   lane already does this — `Engine::new_ipc` in `malachite-app/src/config.rs:54`, selected by
   endpoint config, NOT a CL code change), and/or a cheaper JSON path.
   NOTE: choosing IPC vs HTTP is a *configuration* choice, so it stays inside the constraint.
2. **Persistence** — 116-190 ms/block, the largest measured EL cost after execution. Async, but it
   caps sustainable cadence.
3. **State root** — 5-15 ms. The frozen-root idea lives here. Small.
4. **More execution parallelism** — the verified 4.25x scheme on top of the fast path. Least
   valuable: execution is already the smallest term.

## Verification protocol (unchanged, mandatory)

- Offline gates, BOTH must print IDENTICAL before any deploy:
  `cargo run --release -p arc-evm --example parallel_transfer_bench`
  `ARC_PARALLEL_TRANSFERS=1 cargo run --release -p arc-evm --example parallel_transfer_bench`
- Live: all validators must agree on the payment-lane state root at every height. Divergence halts
  consensus, so a chain that keeps advancing under load is the proof.
- Perf numbers are **state-dependent** (mission 1 saw 7.00 -> 9.08 us/tx purely from chain growth).
  Only compare runs at comparable chain age; `ab-fastpath.sh` starts both arms from a fresh chain.

## Status

- [x] **RECON DONE (iteration 1).** Established, from source:
      * reth's `new_payload_v3` latency metric starts INSIDE the handler
        (`rpc/rpc-engine-api/src/engine_api.rs:220`), i.e. AFTER jsonrpsee deserialises the params.
        The `Block added to canonical chain elapsed=` log is engine-tree time, also post-parse.
        **So neither existing metric sees the ~12 MB JSON parse — it is genuinely unmeasured.**
      * The CL has NO engine-call duration metric (`malachite-app/src/metrics/app.rs` has
        block_time / block_build_time / block_size_bytes but nothing per engine call), and adding
        one would touch the CL => OFF LIMITS.
      * **The payment lane ALREADY SUPPORTS IPC**: `config.rs` builds `EngineConfig::Ipc` when
        `payment_eth_socket` + `payment_execution_socket` are set, and that branch takes PRIORITY
        over the RPC branch. The EVM lane already runs IPC; the payment lane runs authrpc HTTP only
        because that is how `launch-payment-els.sh` / the fleet `payment_el_cmd` configure it.
        **=> switching the payment lane to IPC is CONFIG-ONLY and inside the constraint.**
      CONSEQUENCE: rather than instrument the parse (which needs CL changes), run a TRANSPORT A/B —
      HTTP vs IPC, same everything else — and read the difference in block time / cadence. That
      measures the ingestion cost end-to-end without touching consensus.
- [x] **PER-BLOCK READ CACHES LANDED + VERIFIED (iteration 2, commit below).** Took the state-read
      win WITHOUT batching — ~40 lines instead of ~250. Cached the 3 block-constant reads:
      blocklist status (memoised map; NATIVE_COIN_CONTROL is not written by transfers) and the fee
      beneficiary (full AccountInfo tracked in memory). Both INVALIDATED on any general-EVM tx.
      **5 state reads/transfer -> 2.**
      * Gates: BOTH IDENTICAL on both workloads.
      * Offline executor path: pool 107.7 -> 64.1 -> **54.1 ms**, closed 94.9 -> 58.4 -> **45.4 ms**
        (stock -> fast path -> +caches) = **~2.0x vs stock**, 1.18-1.29x from the caches alone.
      * Live 4-validator demo at 1 Ggas: **122 consecutive heights (154..275), ZERO divergence**;
        exec 89.2 ms @ 5,079 txs/blk = **17.6 us/tx**, vs a prior stock run at a comparable
        5,363 txs/blk = 22.6 us/tx (~1.29x). CAVEAT: those two live runs were not a controlled A/B
        (different chain instances); the controlled evidence is the offline number. Use
        `ab-fastpath.sh` for a rigorous live claim.
      * CORRECTNESS BUG CAUGHT PRE-TEST: the beneficiary cache must hold the FULL AccountInfo —
        rebuilding a default would reset its nonce/code_hash in the state diff and diverge the root.
- [x] **MEASURED (iteration 3): BATCH EXECUTION IS THE WRONG NEXT TARGET — deprioritised.**
      After the fast path + per-block caches, the ~54 ms executor path (47,618 transfers) splits as:
        * 2 state reads/tx ......... **7.2 ms (13%)**  <- all that batch-prefetching could attack
        * pure arithmetic .......... 4.6 ms (9%)
        * **receipts + State/bundle commit ... ~42 ms (78%)**  <- THE REMAINING COST
      So the ~250-line, consensus-critical batch-execution change would chase 13% (and save maybe
      6 ms of it). The caches already took the cheap read win; reads are no longer the problem.
      Probe lives in `parallel_transfer_bench.rs` (prints a "decomposition" block) so this is
      re-checkable after any change.
- [x] **COMMIT-ONCE-PER-BLOCK LANDED + VERIFIED (iteration 4).** Per-block write overlay replaces
      per-tx `db.commit`. Risk settled FIRST by a SAFETY PROBE (500 txs over 64 repeatedly-touched
      accounts, both ways): full BundleState compared — **plain state IDENTICAL, reverts IDENTICAL**.
      That was the check the normal gates could not do (a revert bug breaks reorg unwinding without
      moving the state root).
      * Both gates IDENTICAL on both workloads.
      * Offline executor path, cumulative (pool / closed):
        stock 107.7/94.9 -> fast path 64.1/58.4 -> +caches 54.1/45.4 -> **+overlay 45.3/35.3 ms**
        = **2.23x / 2.51x vs stock**; 1.19x / 1.29x from the overlay alone.
      * Live 4-validator demo at 1 Ggas: **123 consecutive heights (201..323), ZERO divergence**.
        exec 71.2 ms @ 4,254 txs/blk = **16.7 us/tx**; block rate **1.61 blk/s** (previous iteration:
        17.6 us/tx, 1.33 blk/s).
      * HONEST READ: the live exec/tx gain (17.6 -> 16.7, ~1.05x) is far smaller than the offline
        1.19-1.29x, because the executor is a MINORITY of live per-tx cost (state provider + engine
        loop dominate). Block rate did move toward the 2 blk/s target. Neither pair of runs was a
        controlled A/B (different block sizes/chain instances) — for a rigorous live claim use
        `ab-fastpath.sh`.
- [x] **GAP ATTRIBUTED (iteration 5) — SIGNATURE RECOVERY IS THE DOMINANT COST, NOT EXECUTION.**
      No code and no rebuild needed: reth already exposes the split via
      `reth_sync_execution_transaction_execution_histogram` (executor calls) and
      `reth_sync_execution_transaction_wait_histogram` (waiting on the tx iterator). Measured live,
      146 blocks / 870k txs at ~5,962 txs/blk:
        whole execute_transactions loop ... 92.3 ms/blk  (15.48 us/tx)
          waiting for next tx ............ 60.7 ms/blk  (**10.18 us/tx, 66%**) <- SIG RECOVERY
          executor (our code) ............ 21.5 ms/blk  (3.61 us/tx, 23%)
          loop overhead (receipt clone+send, metrics, atomics) 10.1 ms/blk (1.69 us/tx, 11%)
      **Our executor is only 23% of the execution phase.** Four iterations of executor work
      (fast path, read caches, commit-once) took it to 3.61 us/tx; there is little left there.
      ROOT CAUSE FOUND: `payload_validator.rs:323` does `let convert = |tx| tx.try_into_recovered();`
      — reth recovers the signer FROM SCRATCH for every tx in a payload and never consults the
      mempool, even though those txs arrived by gossip and their senders were already recovered at
      pool insertion. Every validator repeats ~47,618 ECDSA recoveries per block.
- [x] **🎯 EAGER PARALLEL SENDER RECOVERY — DONE, 4.0x on the execution phase, NO reth fork (2026-08-08).**
      The item below was right that recovery dominates, but WRONG about the line and the fix, and
      its caveat was wrong too. All three corrections came from measuring first.

      **Correction 1 — wrong line.** `payload_validator.rs:323` is the `BlockOrPayload::Block`
      branch. `newPayload` takes the OTHER branch: `EthEvmConfig::tx_iterator_for_payload`
      (`ethereum/evm/src/lib.rs:300`), which does RLP-decode + `try_recover` per tx. `ArcEvmConfig`
      merely DELEGATED to it — so Arc can override it in arc-evm. **The fork was never needed.**

      **Correction 2 — the caveat was wrong; it is not contention.** `recovery-probe.sh` (new)
      diffs reth's `transaction_execution` / `transaction_wait` histograms per validator. wait/tx
      held at 10.05 vs 10.86 us going from 2 to 8 competing spammers, and at ~10 us under BOTH
      state-root strategies, while our executor's own time moved 2.87 -> 4.14. Structural, not CPU.

      **Correction 3 — it was never "recovery is expensive", it was the PIPELINE.**
      `examples/recovery_bench.rs` (new) measures the floor on this box: decode 0.22 us/tx, ECDSA
      recover 33.42 us/tx serial, **4.02 us/tx across 16 threads (8.5x)**. The live loop realised
      only ~3.3x of that. reth streams recovery through an ordered per-tx channel
      (`spawn_tx_iterator` -> `for_each_ordered_in`), and that delivery — not the cryptography —
      was the limit.

      **Fix (arc-evm only, ~40 lines, env-gated `ARC_EAGER_RECOVERY=1`):** override
      `tx_iterator_for_payload` to recover the whole payload up front on rayon and hand reth an
      already-computed vector. Items are a two-state `PayloadTx::{Done,Raw}`; both funnel through
      one `recover_payload_tx`, so a bad tx surfaces at the same index and the flag-off path stays
      lazy exactly as upstream drives it. Guarded by `EAGER_RECOVERY_MIN_TXS = 30`, mirroring
      upstream's own small-block threshold, so an idle 2 blk/s lane is untouched.

      **MEASURED — same box, same blocks, 4-way A/B (val1 eager+fallback, val2 fallback-only
      control, val3/4 stock):**
      | val | config | loop/tx | wait/tx | exec/tx | exec ms/blk |
      |-----|--------|---------|---------|---------|-------------|
      | 1   | eager + fallback | **3.90 us** | **0.85** | 3.04 | **18.9** |
      | 2   | fallback only    | 15.36 us | 11.99 | 3.37 | 74.6 |
      | 3/4 | stock            | 20.8-21.6 us | 15.8-16.4 | 5.0-5.2 | 101-105 |

      → **3.9x vs the same-config control, ~5.4x vs stock; wait/tx down 93%.** Earlier run at
      ~10.3k txs/blk: 36.9 vs 175.7 ms/blk (4.8x). The execution phase is no longer
      recovery-dominated: wait fell from ~78% to 22% of the loop, and OUR executor is now the
      majority of what remains.

      **Consensus-validated:** 350 consecutive blocks, 1,567,888 txs, all 4 validators identical on
      stateRoot + blockHash + receiptsRoot at EVERY height, with val1 eager against 3 non-eager
      peers. 34 of those blocks fell below the 30-tx threshold, so both branches were exercised.
      A 200-block/1.47M-tx run before the threshold guard also agreed everywhere. Both offline
      gates IDENTICAL, no DIVERGED.

      **Not done / open:** cadence did NOT move (0.76-1.5 blk/s) — as with every EL win so far, it
      shows up in per-block exec_ms, not fleet tps, because ~2.4 s/height is consensus coordination
      (OUT OF SCOPE by the hard constraint). Not yet measured on the 4-machine fleet. Still gated
      off by default. Eager recovery does full recovery work even when tx 0 is invalid, but reth's
      own parallel path already recovers ahead speculatively, so this is not a new DoS surface.

- [ ] ~~NEW TOP PRIORITY: reuse mempool-recovered senders for newPayload txs.~~ **SUPERSEDED above.**
      Still theoretically the last ~4 us/tx (recovery that is now parallel but still performed),
      and it WOULD need the fork plus pool plumbing the payload validator does not currently have.
      Much smaller prize now that the pipeline stall is gone. Original note: up to ~10 us/tx
      (66% of the execution phase) — bigger than everything achieved so far combined. Lives in the
      EL (reth's tx iterator), so it is INSIDE the constraint, but it does need the reth fork
      (`experiments/reth-fork/apply-fork.sh`, already proven to build). Sketch: in
      `tx_iterator_for`, look the tx hash up in the pool and reuse its recovered sender; fall back
      to `try_into_recovered()` on a miss. Correctness is easy to keep — a wrong sender changes the
      state root, so the existing gates + live root agreement catch it immediately.
      CAVEAT on sizing: the `wait` metric is what PARALLEL recovery could not hide, measured on a
      box also running 8 spammers, so the recoverable share may be smaller on the fleet. Re-measure
      there before/after.
- [ ] (superseded) the live/offline gap is now the story. Offline the executor path is 45/35 ms for
      47,618 transfers (~0.8 us/tx) but live exec is ~16.7 us/tx at a fifth the block size. So
      ~95% of live per-tx execution cost is OUTSIDE ArcBlockExecutor — reth's state provider stack
      and engine-tree per-tx loop. Further micro-optimisation INSIDE the executor has little left to
      give (arithmetic is already 0.09 us/tx). MEASURE THAT GAP before optimising anything else.
- [x] (done) cut receipt + commit overhead (78% of what remains). Hypothesis: the
      cost is `db.commit(state)` per transaction on `State<DB>` with bundle tracking — 3 accounts x
      47,618 txs = ~143k TransitionAccount records, each with allocations. Idea: keep a per-block
      overlay of pending account changes in the executor, serve fast-path reads from it, and commit
      ONCE at `finish()` — ~16k transitions instead of 143k. Receipts stay per-tx (they are cheap
      and must keep cumulative-gas order).
      RISK TO SETTLE FIRST (measure before writing): committing once at the end changes how
      BundleState records reverts/original_info. The final PLAIN state (hence the state root) should
      be identical, but revert data matters for reorgs — verify with the gates AND by inspecting the
      bundle, not just balances. Also confirm nothing between txs reads committed state directly.
      **PRE-CHECK DONE (iteration 3) — hypothesis CONFIRMED.** Isolated `db.commit()` with a
      realistic 3-account diff per tx, bundle tracking on. Full decomposition of the ~54 ms:
        * **db.commit() per tx ..... 27.8 ms (51%)  <- THE single biggest cost**
        * receipts + misc .......... ~14.5 ms (27%)
        * 2 state reads/tx ......... 7.1 ms (13%)
        * pure arithmetic .......... 4.6 ms (9%)
      => Committing once per BLOCK instead of once per TX targets 51% of the executor path.
      47,618 commits x 3 accounts = ~143k TransitionAccount records collapse to ~16k (one per
      touched account). Plausible saving ~25 ms of 54 ms (~2x on top of what we already have).

      DESIGN for next iteration (not yet implemented):
        * per-block overlay `HashMap<Address, (AccountInfo original_at_block_start, AccountInfo current)>`
          in the executor; fast path reads overlay-first then DB; fast path writes ONLY the overlay.
        * `commit_transaction` still builds the receipt per tx (cheap, and cumulative-gas order must
          be preserved) but skips `db.commit`.
        * at `finish()`, emit ONE EvmState from the overlay — `Account::from(original_at_block_start)`
          with `info = current` — and commit it once, BEFORE the existing system-contract calls.
        * INVALIDATION: any general-EVM tx must flush the overlay to the DB first (same rule the
          blocklist/beneficiary caches already use), because arbitrary code reads live state.
      WHY THE REVERT SEMANTICS SHOULD HOLD (verify, do not assume): reth keeps reverts per BLOCK
      (`merge_transitions` runs per block), not per tx, and a revert is against the block-start
      value — which the overlay preserves via `original_at_block_start`. Per-tx transition
      granularity is not needed for block-level unwinding. VERIFY by inspecting the bundle
      (reverts + plain state), not just balances, in addition to both gates.
- [ ] (deprioritised, was STEP 1) BATCH EXECUTION

 inside ArcBlockExecutor.** This is the unlock
      for every other execution win, and nothing fundamental blocks it — the earlier "can't batch"
      note applied to reth's GENERIC side (it cannot construct `E::Result`); inside arc-evm the types
      are concrete.
      WHY: measured layering of live execution shows compute is ~1% of cost —
        pure transfer arithmetic 0.09 us/tx | + executor machinery 1.35 | + live node 7-9.
      ~85% is STATE READS + per-tx bookkeeping. The fast path does 5 reads/transfer:
      basic(recipient), basic(sender), basic(beneficiary), blocklist SLOAD(sender),
      blocklist SLOAD(recipient). **3 of the 5 are constant for the whole block** (beneficiary never
      changes; the blocklist contract is not written by transfers) => cache once per block, 5 -> 2.
      Batching additionally enables: ONE parallel prefetch pass for every touched account, the
      verified 4.25x sender-partitioned parallel scheme (needs the full tx list), and amortised
      receipt/bookkeeping work.
      DESIGN (already validated in mission 1): buffer during VALIDATION only
      (`!ctx.extra_data.is_empty()`, the discriminator Arc's own finish() uses) so the BUILDER — which
      inspects per-tx results for inclusion — stays strictly per-tx; execute the batch in `finish()`;
      feed results through the EXISTING `commit_transaction` in ORIGINAL tx order so receipts/gas/
      bloom remain production code. The engine loop ignores the per-tx return value and tolerates
      receipts appearing only at the end, and reth's receipt-root task FAILS CLOSED (0 streamed
      receipts -> returns nothing -> validator computes the root from final receipts). All verified.
      MUST PRESERVE EXACTLY (or the root diverges and consensus halts):
        * per-tx block gas-limit check at the same position in the sequence;
        * nonce order within each sender;
        * identical error semantics — safest is: any ineligible/failing tx aborts the batch and the
          whole block re-runs serially;
        * receipt order + cumulative gas in ORIGINAL tx order.
- [ ] (deprioritised) transport A/B — payment lane over HTTP vs IPC.** Needs: payment EL to
      expose an IPC socket on a shared volume, CL flags `--payment-eth-socket` /
      `--payment-execution-socket` pointing at it (both already exist), and the socket mounted into
      both containers. Compare block time + cadence at full 1-Ggas blocks, same load.
- [ ] (superseded) measure engine-API ingestion cost directly. Timestamp the CL's `newPayload` request against
      reth's reported `elapsed`, at full 1-Ggas blocks. Cheapest route: reth debug/trace logs on the
      authrpc handler, or compare CL-side round-trip time vs EL-side elapsed.
- [ ] STEP 2: if large — try IPC for the payment lane (config-only) and re-measure.
- [ ] STEP 3: whatever the data says next (persistence tuning, frozen root, parallel exec).

## Rules
- Small verified increments; never leave the repo broken or a chain half-deployed.
- Tear down cleanly if the box is left idle.
- Record findings here and in CLAUDE.md every iteration, including negative results.


## ❌ MEMORY GROWTH IS CONFIG-INDEPENDENT — AND MY "16% FROM PERSISTENCE" WAS A MACHINE ARTIFACT (2026-08-09, iter 11)

**RETRACTION FIRST.** Last iteration I reported that lowering the persistence threshold slowed
memory growth 16% (val1 298 vs val2 353 MiB/min, matched 62 GB hardware). That was wrong. I never
checked what the SAME machine pair does UNTREATED: in the all-stock baseline ginny already grew 15%
slower than ginnythui (233 vs 274, ratio 0.850). The persistence run's ratio was 0.844. **The
treatment effect was -0.7%, i.e. nothing.** Cross-machine A/B requires the untreated ratio for that
pair as the control; I compared treated-vs-control across different boxes and read the hardware
difference as a result.

**THIS ITERATION.** Probed the remaining large caches, per validator, same chain/load/size, 35 min:
val1 `--engine.cross-block-cache-size 256` (down from a 4096 MB default, 16x smaller), val4
`--engine.disable-state-cache` (off entirely), val2/val3 stock.

| run | ginny | thui | pduck | alien | ginny/thui | effect vs baseline |
|-----|-------|------|-------|-------|------------|--------------------|
| all-stock baseline | 233 | 274 | 323 | 194 | 0.850 | — |
| persistence 2/4 | 298 | 353 | 415 | 288 | 0.844 | **-0.7%** |
| cache 256 MB / cache OFF | 294 | 348 | 411 | 283 | 0.845 | **-0.7%** |

**NEITHER KNOB DOES ANYTHING.** Shrinking the cross-block cache 16x, and disabling the state cache
outright, change the growth rate by under 1% once normalised. The three runs' absolute rates are
also within ~1.5% of each other (294/348/411 vs 298/353/415), across three different configs.
**The growth is config-independent, so it is not any cache that reth exposes a flag for.**

**WHAT IT SCALES WITH.** Consistent across all three runs and every config: **+3,060-4,440 MiB per
1000 blocks**, i.e. **~0.65-0.93 KB per transaction processed** (blocks are 4,761 tx). Growth
tracks transactions, not time and not blocks. That is the signature of per-transaction retention
that is never released — a leak, or an unbounded structure with no knob.

**CONFIG IS EXHAUSTED FOR THIS PROBLEM.** Three candidate mechanisms are now eliminated by
measurement (in-memory block buffer, cross-block state cache, state cache entirely). What remains
needs a heap profile of the payment EL under load, or an upstream reth question — a different kind
of work from anything in this mission, and not config-only.

**OPERATIONALLY, UNTIL THEN:** budget >=24 GB per payment EL at 100M, and expect to restart ELs
periodically under sustained load; a node at its cap dies and silently corrupts throughput
measurements before it does (that is how the 200M frontier row came out wrong).

## ❌ PERSISTENCE THRESHOLD IS NOT THE MEMORY CULPRIT — ~16%, NOT A FIX (2026-08-09, iter 10)

Tested the mitigation named last iteration: LOWER the persistence threshold so the in-memory block
buffer is retired sooner (the earlier experiment RAISED it and hurt cadence). Config-only, per
validator: val1 + val4 got `--engine.persistence-threshold 2 --engine.memory-block-buffer-target 4`,
val2 + val3 stayed stock. Same chain, same blocks, identical load, 35 min.

| node | RAM | config | start -> end | rate | per 1000 blk |
|------|-----|--------|--------------|------|--------------|
| val1 ginny | 62 GB | **LOW** | 462 -> 9,700 MiB | **298 MiB/min** | 3,179 MiB |
| val2 ginnythui | 62 GB | stock | 1,042 -> 11,981 MiB | 353 MiB/min | 3,764 MiB |
| val3 papaduck | 78 GB | stock | 3,033 -> 15,892 MiB | 415 MiB/min | 4,425 MiB |
| val4 alien2 | 15 GB (11 GiB cap) | **LOW** | 689 -> 9,618 MiB | 288 MiB/min | 3,073 MiB |

**[RETRACTED -- see the iteration-11 entry above: normalised against the untreated ratio for this machine pair the effect is -0.7%, i.e. nothing.]** ~~MATCHED-HARDWARE RESULT (val1 vs val2, both 62 GB): 298 vs 353 MiB/min = 16% slower.~~ Real, and
in the right direction — but every node is still marked STILL CLIMBING and still adding
3.1-4.4 GiB per 1000 blocks. **So the in-memory block buffer accounts for only ~16% of the growth;
~84% is something else. The leading hypothesis is ruled out.**

val4 (the 11 GiB canary) SURVIVED this window where it OOMed in the stock run — but it finished at
9,618 MiB and was ~6 minutes from the cap at its measured rate. **The flag buys time proportional to
the 16%, not safety.** Do not treat it as a fix.

Weak secondary signal, worth one line: comparing the two STOCK nodes, the 78 GB machine grew 17%
faster than the 62 GB one (415 vs 353 MiB/min), which is consistent with reth sizing some cache
against available RAM. Confounded with hardware, so it is a hypothesis, not a result.

NEXT (still EL-side, still in scope): the buffer is exonerated, so the candidates are reth's
state/trie caches or a genuine leak. Cheapest next step is to look for cache-sizing flags on the
node binary and A/B those the same way; a heap profile would settle it but is a larger lift.
Whatever runs next, `fleet/mem-soak.sh` must run alongside it — a node quietly approaching its cap
distorts throughput measurements before it dies.

## 🚨 PAYMENT-EL MEMORY GROWS UNBOUNDED UNDER SUSTAINED LOAD — OOM REPRODUCED AND PREDICTED (2026-08-09, iter 9)

Followed up the OOM flagged last iteration instead of chasing more tps. It is real, systematic, and
a bigger problem than any throughput number here.

45 min of continuous load at the recommended 100M operating point (~8.5k tx/s offered, blocks
100% full), sampling every payment EL's container memory once a minute
(`experiments/dual-el/fleet/mem-soak.sh`):

| node | RAM | start | after 35 min | rate | per 1000 blocks | verdict |
|------|-----|-------|--------------|------|-----------------|---------|
| ginny (local) | 62 GB | 4,298 MiB | 11,766 MiB | +233 MiB/min | +2,525 MiB | STILL CLIMBING |
| ginnythui | 62 GB | 5,585 | 14,356 | +274 MiB/min | +2,965 MiB | STILL CLIMBING |
| papaduck | 78 GB | 8,011 | **18,340** | +323 MiB/min | +3,492 MiB | STILL CLIMBING |
| papaduck-alien2 | 15 GB, **11 GiB cap** | 4,328 | **OOM-KILLED** | — | — | `oom=true exit=137` |

**PREDICTED THEN CONFIRMED.** At minute 20 alien2 sat at 8,964 MiB and was climbing ~194 MiB/min,
so I predicted it would cross its 11,264 MiB cap around minute 32-35 — it did, `oom=true exit=137`.
That is the same failure that corrupted the 200M frontier row, so the earlier OOM was not a fluke.

**GROWTH DECELERATES BUT DOES NOT PLATEAU.** Local went 288 -> 209 -> 168 MiB/min across the run,
yet the last third still added +2,100 MiB. After 2,958 blocks / 35 min nothing had flattened, and
papaduck reached 18.3 GiB for a chain of 16k accounts and ~3k blocks. Whether this is a true leak
or cache growth that eventually bounds, it is far past any reasonable working set for this workload.

**CONSEQUENCES**
- **Any node capped below the trajectory dies.** 11 GiB is not enough at 100M; the three uncapped
  nodes were at 11.8-18.3 GiB after 35 min and rising. Budget >=24 GB for a payment EL at this
  load, or find the cause.
- **It invalidates long-run measurements silently.** A dying node slows the fleet, which makes
  blocks fill, which looks like a *capacity* result. That is exactly how the 200M row came out as
  "100% full at 0.61 blk/s, sd 17.4%, -20.4% drift". Any sustained run from now on must sample
  memory alongside throughput.
- **It is EL-side and therefore in scope** — unlike everything else remaining on this mission.

**NOT YET DIAGNOSED.** Hypotheses in rough order: reth's in-memory canonical block/state buffer
growing faster than persistence retires it; per-block bundle/trie overlays retained; and (weakest)
mempool backlog from the ~1,700 tx/s of surplus offered load, which the 200k pending/queued caps
should bound to a few hundred MiB. NOTE the earlier persistence experiment RAISED the threshold
(64/128) and made cadence worse; the untested direction is LOWERING it so the in-memory buffer is
retired sooner. That is the obvious next test and it is config-only.

## 🎯 SUSTAINED FRONTIER — 15 MIN PER SIZE, 4 MACHINES (2026-08-09, iter 8)

The definitive measurement: one gas limit at a time, held under continuous load for a full 15
minutes, chain sampled every 60 s, demand tuned per size to ~1.15x its expected capacity.
Harness `experiments/dual-el/fleet/frontier-long.sh`, chart `frontier-long-chart.py`.

| gas | txs/blk | %full | latency (min-max) | throughput | spread | exec | root | persist |
|------|---------|-------|-------------------|------------|--------|------|------|---------|
| 25M | 1,190 | 100% | **520 ms** (505-532) | 2,287 tx/s | 1.6% | 12.0 | 1.3 | 48.6 |
| 50M | 2,380 | 100% | 537 ms (527-566) | 4,430 tx/s | 1.8% | 26.0 | 2.2 | 61.7 |
| **100M** | 4,761 | 100% | **697 ms** (662-779) | **6,827 tx/s** | 3.6% | 56.9 | 2.6 | 97.2 |
| 200M | 4,892 | 51% | 690 ms (524-1176) | 7,092 tx/s | 3.2% | 63.2 | 3.4 | 203.6 |

All four validators agreed at every size (50-block check per size).

**FINDING 1 — THE GAS LIMIT IS A DIAL THAT ONLY ACTS WHILE IT BINDS.** At 200M the same demand no
longer fills the block, so 200M lands on top of 100M: 4,892 vs 4,761 txs/block, 690 vs 697 ms,
7,092 vs 6,827 tx/s. **Raising the limit past what demand can fill changes nothing at all.** This
reframes every earlier "bigger blocks" result: the limit is a CAP, and only the binding case is a
measurement of the limit. Forcing 200M to bind (2x demand, short runs) gives 9,523 txs at ~1,230 ms
and ~7,700 tx/s -- latency nearly doubles for ~10% more throughput.

**FINDING 2 — LONG WINDOWS MATTER, AND THEY REFINE THE HEADLINE.** Spread over 15 min is 1.6-3.6%,
against the +-12-15% short windows carry here. The refined numbers move: 50M is **537 ms / 1.86
blk/s**, not the 1.92-1.94 short runs suggested, so **25M (520 ms) is the only size that holds
2 blk/s** and even it is 1.92, not 2.00. The earlier "50M holds 2 blk/s" claim is hereby corrected
for the second and final time -- it is a 537 ms operating point.

**FINDING 3 — A LONG-RUN MEMORY LEAK ON THE SMALLEST NODE, NOT A BLOCK-SIZE CLIFF.** During the
first 200M window val4's payment EL (papaduck-alien2, 15 GB RAM, 11 GB container cap) was
OOM-killed (`oom=true exit=137`) ~6 min in, after surviving 15-min windows at 25/50/100M. That run
is therefore INVALID -- its "100% full at 0.61 blk/s, sd 17.4%, -20.4% drift" was the signature of
a dying node, not of 200M. **The OOM did NOT reproduce**: a fresh process at 200M ran 13 min clean
(sd 3.2%, no drift). So it is cumulative memory growth over ~1.5 h of sustained load crossing an
11 GB cap, not a property of 200M. Worth tracking, but do not report it as a block-size limit.

**FINDING 4 — the commitment still never enters the tradeoff.** State root 1.3-3.4 ms at every
size, flat across a 8x range of block size. Persistence is the EL cost that actually scales
(48.6 -> 203.6 ms).

**OPERATING RECOMMENDATION: 100M / ~6,800 tx/s / ~700 ms** as the throughput point, or **25M /
2,287 tx/s / 520 ms** if the promise is literally 2 blocks per second. Both sustained for 15
minutes with all validators agreeing.

## 📊 DECK ALIGNED WITH THE MEASUREMENTS (2026-08-09, iter 7)

The deck had been headlining 1 Ggas at "9.5k / 15k tx/s" as the throughput achievement. That is the
number that produced the standing impression that bigger blocks bought throughput. Rewritten to
compare 100M against 1 Ggas directly (7,398 tx/s @ 644 ms vs 7,272 @ 5,478 ms) and to carry the
marginal-cost table, which is the mechanism. Also corrected a bullet of mine that claimed state root
was "20-50 ms at 9.5k tx/block" -- that mixed the synchronous and asynchronous measurements; on the
fleet it is 2.3 ms at 4,761 tx and 0.3 ms at 39,832, and it does not grow with block size. The
non-reproduction of the 9.5k/15k figure is stated on the slide.

NO further EL experiment was run this iteration: mission 3 is closed and every ranked lead is a
measured negative or a measured knee. Running another sweep would be manufacturing work.

## 🔬 WHERE A BIGGER BLOCK'S MILLISECONDS GO — MEASURED, AND MISSION 3 CLOSED (2026-08-09, iter 6)

Two things this iteration: (a) the 50M "holds 2 blk/s" headline REPRODUCES, and (b) the marginal
cost of a bigger block is now attributed, using Arc's own `reth_arc_payload_total_duration_seconds`
(proposer build) alongside the beacon-engine metrics. Both points 100% full, 4 machines,
distributed spam.

| gas | txs/blk | height | build | newPayload | exec | root | vote gap | remainder | tps |
|------|---------|--------|-------|-----------|------|------|----------|-----------|-----|
| 50M | 2,380 | 515 ms | 68.1 | 43.5 | 35.9 | 3.0 | 347.6 | 124.2 | 4,619 |
| 200M | 9,523 | 1,216 ms | 176.7 | 124.8 | 117.2 | 1.7 | 836.2 | 254.5 | 7,834 |

**REPRODUCIBILITY CONFIRMED: 50M = 1.94 blk/s / 515 ms / 4,619 tps**, against 1.92 / 522 / 4,563
measured on a different chain instance. Within 1.5%. The mission headline stands (this check
mattered — an earlier single-run 50M claim had to be requalified when it failed to repeat).

**THE MARGINAL COST OF A TRANSACTION IS ~98 us OF HEIGHT — and only ~11 us of it is our execution:**

| component | +ms (50M -> 200M) | us / extra tx | share of the growth |
|-----------|-------------------|---------------|---------------------|
| proposer build | +108.6 | 15.2 | 15.5% |
| newPayload (own execution) | +81.3 | 11.4 | 11.6% |
| **vote gap** | **+488.6** | **68.4** | **69.7%** |
| remainder (stream + decode) | +130.3 | 18.2 | 18.6% |

**~70% of what a bigger block costs lands in the VOTE GAP** — the window from this validator
finishing `newPayload` to the next forkchoiceUpdated arriving. That window is not idle network
time: it contains the OTHER validators receiving, decoding and executing the same block, then two
vote rounds. So each transaction is effectively executed ~5x across the network (proposer builds it
once, four validators validate it) and every one of those executions sits on the critical path of
the round, with the quorum gated by the SLOWEST of them.

That is also why the state root cannot be the answer: it is 1.7-3.0 ms and it does not grow with
block size (1.7 ms at 9,523 txs vs 3.0 ms at 2,380 — noise, not scaling).

**MISSION 3 IS CLOSED.** Goal was "a block larger than 25M that still holds 2 blk/s": **50M does,
at 4,619 tps (1.95x the 2,363 baseline), 100% full, all four validators agreeing.** Best overall
operating point is 100M at 7,398 tps / 644 ms. All four ranked leads are closed: persistence is a
measured negative (it inflates state-root cost), the fine-grained sweep found the knee, the
delivery-bound flaw is fixed by distributed load, and state-root-fallback was applied throughout.

**No further tps at 2 blk/s is reachable without touching consensus.** The evidence is the table
above: 88% of the marginal cost of a transaction is outside our execution, in build + stream +
vote. The addressable levers are all in the CL path — ship transaction hashes instead of full
transactions (peers already hold them in their mempools, so the proposal duplicates ~1.2 MB at
100M), pipeline execution of height N against consensus on N+1, and homogenise the validator set so
the quorum is not gated by the slowest machine.

## ✅ MISSION 3 GOAL MET ON REAL HARDWARE — 50M HOLDS 2 blk/s AT 4,563 TPS (2026-08-09, iter 5)

Measured the fleet's LOW-LATENCY end, which had never been tested (previous fleet points started at
200M). 4 machines, distributed spam, stock config:

| gas | spammers | txs/blk | %full | blk/s | latency | tps | exec | root | persist |
|------|----------|---------|-------|-------|---------|-----|------|------|---------|
| 25M | 16 | 1,190 | 100% | 1.89 | 529 ms | 2,249 | 11.4 | 0.9 | 48.2 |
| **50M** | 16 | **2,380** | **100%** | **1.92** | **522 ms** | **4,563** | 22.9 | 1.2 | 52.9 |
| 100M | 16 | 2,845 | 60% | 1.81 | 553 ms | 5,144 | 29.6 | 2.3 | 91.1 |
| **100M** | 32 | **4,761** | **100%** | 1.55 | **644 ms** | **7,398** | 50.0 | 2.3 | 109.3 |

**GOAL MET: 50M gas — twice the 25M baseline — holds 2 blk/s at 1.92 blk/s / 522 ms with 100% full
blocks, at 4,563 tps (1.93x the 2,363 tps baseline).** All four validators agreeing throughout.
This is the mission's success criterion, on real hardware, load-saturated.

**BEST OVERALL POINT: 100M saturated — 7,398 tps at 644 ms.** That beats every larger block on BOTH
axes: more tps than 200M (7,150 @ 864 ms) and than 1 Ggas (7,272 @ 5,478 ms), at a fraction of the
latency. **100M -> 1 Ggas is 10x the block for ZERO extra throughput and 8.5x the latency.**

So the fleet frontier has a knee at ~100M, and everything beyond it is pure latency cost. The
operating rule is "smallest block that reaches the plateau", not "biggest block that fits".

Note the two 100M rows are the over-offering tradeoff again, and here it is worth taking: 16 -> 32
spammers costs 553 -> 644 ms (+16%) and buys 5,144 -> 7,398 tps (+44%). At 200M the same doubling
bought only +6% tps for +45% latency. The tradeoff is favourable at the knee and unfavourable past
it, which is another way of saying where the knee is.

## 🎯 FLEET FRONTIER — RECONCILES THE "10k tps" CLAIM WITH THE 4.7k SINGLE-BOX NUMBER (2026-08-09, iter 4)

Question raised: the deck says 9.5k tps at 1 Ggas, the single-box sweep tops out at 4,748. Both are
real; they are different hardware AND different block sizes. Re-measured on the actual 4-machine
fleet (ginny + ginnythui + papaduck + alien2), stock config, distributed spam (one spammer set per
machine against its LOCAL payment EL) so the comparison with the recorded 9.5k is apples-to-apples:

| gas | spammers | txs/blk | %full | blk/s | latency | tps | exec | root | persist |
|------|----------|---------|-------|-------|---------|-----|------|------|---------|
| 200M | 16 | 6,175 | 65% | **1.16** | **864 ms** | **7,150** | 61.8 | 2.3 | 66.9 |
| 200M | 32 | 9,523 | 100% | 0.80 | 1,250 ms | 7,616 | 98.9 | 2.3 | 80.1 |
| 500M | 16 | 15,430 | 65% | 0.53 | 1,876 ms | **8,226** | 177.0 | 1.0 | 101.0 |
| 1 Ggas | 16 | 39,832 | 84% | 0.18 | 5,478 ms | 7,272 | 479.3 | **0.3** | 210.1 |

**FINDING 1 — on the fleet, THROUGHPUT SATURATES near 7-8k tx/s and only LATENCY changes.** Across
a 5x block-size range (200M -> 1 Ggas) tps moves 7,150 -> 7,272 (i.e. not at all, within variance)
while latency goes 864 ms -> 5,478 ms, **6.3x worse**. Bigger blocks buy nothing on real hardware.
**So the right operating point is the SMALLEST block that reaches the plateau: 200M at ~864 ms.**

**FINDING 2 — the 9.5k/4.7k gap was hardware + block size, not a regression.** Single box runs all
four validators (12 containers) on 16 shared cores and every validator re-executes every block; the
fleet gives each validator its own machine. At the same 200M the fleet does **7,150 tps @ 864 ms**
vs the single box's 6,785 @ 1,404 ms — same throughput plateau, **1.6x better latency**.

**FINDING 3 — OVER-OFFERING LOAD IS COUNTERPRODUCTIVE, quantified.** Doubling spam at 200M (16 ->
32 spammers) filled blocks 65% -> 100% and bought +6% tps (7,150 -> 7,616) while costing +45%
latency (864 -> 1,250 ms). Filling the block is NOT the goal; the ingress cost of the surplus
(admission + gossip for txs that will not fit) exceeds what the extra fullness returns. This is the
same effect measured single-box (50M: 1.96 blk/s light-load vs 1.47-1.61 saturated) and it means
**"100% full" is the wrong success criterion for a latency-sensitive lane.**

**FINDING 4 — the paper's thesis gets STRONGER at scale.** State root on a 39,832-tx block:
**0.3 ms.** On 15,430 txs: 1.0 ms. The commitment cost stays flat-to-negligible as blocks grow by
5x, exactly as claimed. Execution is what scales (61.8 -> 479.3 ms).

Today's 1 Ggas number (7,272 tps) is somewhat below the recorded 9.5k avg / 15k peak; that run was
72% full at 34,363 txs/blk against today's 84% at 39,832. Different chain age and machine state;
both sit on the same 7-9k plateau. Peak-vs-average also differs — 15k was a peak, 7.3k here is a
120 s average.

## 🎯 2-D SWEEP: BLOCK TIME x GAS — BLOCK TIME IS NOT A THROUGHPUT KNOB (2026-08-09, iteration 3)

First sweep where EVERY point is 100% full: distributed load (2 local + 8 ginnythui + 5
papaduck-alien2 spammers over tailscale, against this box's payment EL). Local-only load saturates
at ~6,000 tx/s and could never fill 100M+ blocks, which is what invalidated every previous
high-gas row. Harness: `experiments/dual-el/blocktime-sweep.sh`, chart `blocktime-chart.py`.

| target | gas | txs/blk | blk/s | latency | tps | exec | root | persist | verdict |
|--------|-----|---------|-------|---------|-----|------|------|---------|---------|
| 250 ms | 50M | 2,380 | 1.61 | 620 ms | 3,840 | 33.7 | 30.5 | 63.0 | missed |
| 250 ms | 100M | 4,761 | 1.10 | 910 ms | 5,234 | 83.9 | 44.5 | 86.5 | missed |
| 250 ms | 200M | 9,523 | 0.71 | 1404 ms | **6,785** | 174.1 | 47.9 | 127.2 | missed |
| 500 ms | 50M | 2,380 | 1.47 | 680 ms | 3,500 | 32.5 | 31.5 | 160.2 | missed |
| 500 ms | 100M | 4,761 | 1.06 | 947 ms | 5,025 | 78.5 | 44.1 | 161.6 | missed |
| 500 ms | 200M | 9,523 | 0.68 | 1460 ms | 6,522 | 160.6 | 51.7 | 243.7 | missed |
| 1000 ms | 50M | 2,380 | 1.00 | 1001 ms | 2,378 | 21.7 | 20.7 | 66.1 | **HELD** |
| **1000 ms** | **100M** | **4,761** | **1.00** | **1003 ms** | **4,748** | 75.5 | 45.7 | 111.1 | **HELD** |
| 1000 ms | 200M | 9,523 | 0.67 | 1492 ms | 6,382 | 162.7 | 49.1 | 265.4 | missed |

**FINDING 1 — the target block time buys NOTHING.** Under saturation the chain runs at its natural
cadence, set by the GAS LIMIT. Compare the same gas across targets: 50M gives 1.61 / 1.47 / 1.00
blk/s at 250 / 500 / 1000 ms. Asking for 250 ms instead of 500 ms changes nothing (the chain is
already slower than both); asking for 1000 ms actively THROTTLES it (50M could do ~1.5 blk/s and is
paced down to exactly 1.00, costing ~1,100 tps). **`targetBlockTimeMs` is a ceiling, never a floor.**
It is a product/latency-predictability knob, not a performance one.

**FINDING 2 — the gas limit IS the frontier, with steep diminishing returns.** Natural cadence
(averaging the unpaced 250/500 rows): 50M ~1.54 blk/s / ~3,670 tps; 100M ~1.08 / ~5,130; 200M ~0.70
/ ~6,650. So **4x the gas buys 1.8x the tps and costs 2.2x the latency.**

**FINDING 3 — offered load beyond what fills a block STILL costs cadence.** At 50M with 6 LOCAL
spammers the chain did 1.96 blk/s; at the same 50M and the same 100%-full blocks, with 15
distributed spammers, it does 1.47-1.61. Block composition is identical, so the delta is pure
INGRESS cost: RPC/mempool admission and gossip for transactions that will not fit anyway. This
retro-explains why the earlier "40M holds 2 blk/s" reading was obtained under light load — it is
real, but it is a light-load number.

**FINDING 4 — the only configuration that both saturates and HOLDS its target is 100M @ 1000 ms:
4,748 tps at a stable, predictable 1.00 blk/s**, with ~8% cadence headroom (natural ~1.08). That
headroom is what makes it hold. 200M is faster on paper (6,785 tps) but holds no target at all and
lands at 1.4-1.5 s blocks.

**ANSWER TO "best tps at 1 blk/s": 4,748 tps at 100M gas, held at exactly 1.00 blk/s.**
**ANSWER TO "2 blk/s": NOT reachable at any gas size under saturating distributed load** — the
fastest saturated point in the whole sweep is 1.61 blk/s (620 ms) at 50M, and that one misses its
own 250 ms target. 2 blk/s remains reachable only at <=40M with light offered load.

## ⚠️ REQUALIFICATION + THE LOAD GENERATOR IS NOW THE CEILING (2026-08-09, iteration 2)

**50M does NOT reliably hold 2 blk/s — it is MARGINAL.** A reverse-order sweep on a fresh chain
(60 -> 55 -> 50, so the biggest block got the freshest chain) gave:

| gas | forward sweep | reverse sweep |
|------|---------------|---------------|
| 50M | **1.96 blk/s / 511 ms** | **1.72 blk/s / 582 ms** |
| 55M | 1.69 / 591 ms | 1.62 / 615 ms |
| 60M | 1.75 / 573 ms | 1.58 / 631 ms |

Same size, same load, same config, ~12% apart. So the earlier chain-age suspicion was WRONG — the
reverse order ruled it out — and the real explanation is plain run-to-run variance (this box has
always shown +-15%). **The reproducible holding point is 40M: 1.98 blk/s, 504 ms, 3,777 tps.**
50M should be quoted as "marginal, 1.7-2.0 blk/s", not as the ceiling. The headline from iteration 1
was over-fitted to a single run.

**THE 1 blk/s QUESTION CANNOT BE ANSWERED ON THIS BOX — the spammer, not the chain, is the limit.**
Sweep at 100/200/300/400M with **12** spammers:

| gas | txs/blk | blk/s | tps | %full |
|------|---------|-------|------|-------|
| 100M | 3,183 | 1.72 | 5,468 | **67%** |
| 200M | 3,807 | 1.53 | 5,831 | **40%** |
| 300M | 5,393 | 1.15 | 6,179 | **38%** |
| 400M | 3,649 | 1.60 | 5,832 | **19%** |

Every row is delivery-bound, and doubling spammers 6 -> 12 barely moved delivered tps (5,186 ->
~5,500-6,200): local load generation saturates around **~6,000 tx/s** because the spammers compete
with 12 containers for 16 cores. This reproduces the mission-1 finding that MORE spammers made it
worse. Those cadence numbers (e.g. 300M at 1.15 blk/s) are produced by PARTIAL blocks and are
therefore NOT capacity measurements — they must not be quoted as a 1 blk/s result.

To answer it properly needs distributed load (fleet/spam-fleet-distributed.sh). Attempted; blocked
because `tailscale ssh` now requires interactive re-auth. ginnythui and papaduck-alien2 are online,
papaduck is absent from the tailnet.

**BEST SATURATED tps MEASURED TO DATE: 7,164 tps at 100M / 665 ms / 1.50 blk/s, 100% full** (the
earlier 4-spammer coarse sweep). Notably HIGHER than today's 12-spammer attempt at the same size —
more load generators made the measurement worse, not better.

## 🎯 MISSION 3 RESULT: 50M GAS HOLDS 2 blk/s AT 4,660 TPS — 1.97x THE BASELINE (2026-08-09)

Goal was "a block larger than 25M that still holds 2 blk/s". Achieved, and the win came from
FIXING THE MEASUREMENT, not from optimising anything.

Fine-grained sweep, one chain, gas flipped at runtime, all 4 payment ELs on
`--engine.state-root-fallback`, 75 s windows, **6 spammers so every point is load-saturated
(100% full)**:

| gas | txs/blk | blk/s | latency | tps | exec | root | persist | |
|------|---------|-------|---------|------|------|------|---------|--|
| 25M | 1,190 | 1.99 | 504 ms | 2,363 | 3.8 | 10.5 | 49.0 | HOLDS (prior sweep) |
| 30M | 1,428 | 1.99 | 504 ms | 2,835 | 15.0 | 12.5 | 54.9 | HOLDS |
| 40M | 1,904 | 1.98 | 504 ms | 3,777 | 28.0 | 19.3 | 61.0 | HOLDS |
| **50M** | **2,380** | **1.96** | **511 ms** | **4,660** | 42.4 | 27.8 | 70.4 | MARGINAL — see requalification above (1.72 on repeat) |
| 55M | 2,618 | 1.69 | 591 ms | 4,427 | 49.2 | 30.9 | 157.0 | degraded |
| 60M | 2,856 | 1.75 | 573 ms | 4,986 | 57.3 | 31.7 | 77.3 | degraded |
| 75M | 3,571 | 1.45 | 689 ms | 5,186 | 64.3 | 34.8 | 165.2 | degraded |

**The old 25M answer was an artefact of under-delivery.** The previous sweep used 4 spammers; at
50M it read 782 ms / 1.28 blk/s with persist "spiking" to 448.6 ms. Re-measured with 6 spammers:
**511 ms / 1.96 blk/s, persist 70.4 ms.** The spike was noise, and it was the entire basis for
lead #1. Doubling deliverable throughput at the latency target required no code at all.

CONFOUND (stated, not smoothed): sizes are swept sequentially on a GROWING chain, so later points
carry more state. 55M ran last (~1,700 blocks in) and came out worse than 50M in BOTH tps and
latency — some of that is chain age, not size. The true ceiling on a fresh chain may sit slightly
above 50M. Above 50M tps also flattens hard (4,660 -> 4,986 -> 5,186) while latency climbs, so
50M is close to optimal on both axes regardless.

## ❌ LEAD #1 (PERSISTENCE) — CLOSED, IT MAKES THINGS WORSE (2026-08-09)

Same sweep sequence re-run with `--engine.persistence-threshold 64 --engine.memory-block-buffer-target 128`
on all 4 (a storage-timing knob; it cannot change execution semantics, and a cadence experiment
requires all validators since cadence is a chain property):

| gas | default | tuned |
|------|---------|-------|
| 30M | 1.99 blk/s, 2,835 tps | 1.93, 2,758 |
| 40M | 1.98, 3,777 | 1.65, 3,146 |
| 50M | **1.96, 4,660** | 1.36, 3,232 |
| 60M | 1.75, 4,986 | 1.13, 3,235 |

**Worse at every size, and the mechanism is visible: STATE ROOT went 2-3x more expensive**
(12.5/19.3/27.8/31.7 -> 43.6/63.8/61.0/69.6 ms) while persist did NOT drop. Holding 64-128 blocks
in memory forces root computation to walk a much deeper in-memory overlay. Deferring persistence
relocates cost into state root.

Supporting evidence that persistence was never the limiter: fsync on this box is **1.28 ms/op**
(4k dsync), persist is async, and persist never correlated with cadence in the original data
(50M persist 448 ms -> 782 ms cadence, but 100M persist 74.9 ms -> 665 ms).

## 🚨 FOURTH CONSENSUS BUG: ZERO-VALUE TRANSFERS EMITTED A LOG — FOUND BY THE NEW GATE (2026-08-09)

Clearing the correctness debt (EIP-2930 with empty access list + zero-value transfers added to
`Workload::Mixed`) immediately caught a fourth fork: state digests matched, **receipts did not** --
stock 6,538 logs vs fast path 7,000. The 462 difference was exactly the zero-value transfers.

`ArcEvm::before_frame_init` only reaches the log builder through
`Some((from, to, amount)) if !amount.is_zero()` — **a zero-value transfer emits NOTHING.** The fast
path emitted one unconditionally. Same silent-fork shape as the missing-log and legacy-fee bugs:
receipts move, state does not.

FIXED (`executor.rs`): return no logs when `value.is_zero()`, ahead of the hardfork split.
Both gates now agree on every digest. EIP-2930-with-empty-access-list needed no fix — the earlier
`effective_gas_price` change already covers it.

Scope note: this fix is OFFLINE-validated. The live spammer never sends zero-value transfers, so a
live run would not exercise it; the gate is the appropriate test. The flag remains OFF by default.

**FOUR bugs, ONE shape.** Every fast-path bug came from assuming what a transaction is instead of
asking: assumed logs always, assumed 1559 fees, assumed a log for every transfer. Three of the four
were invisible to state comparison alone.

## 🚨 SECOND CONSENSUS BUG IN THE FAST PATH: LEGACY-TX FEES — FOUND, FIXED (2026-08-08)

Hunting the same class of blind spot as the log bug (both gates only ever ran **pure EIP-1559
transfer** blocks) turned up a second fork.

Live mixed load (`--mix transfer=60,erc20=25,guzzler=10,legacy=5`), val1 fast path vs val2-4 stock:
val1 diverged at block 50 on the **STATE ROOT** (not receipts) and stalled at 49 while the network
ran on to 125.

```
mismatched block state root:
  got 0x552d6799…  expected 0x8a4de677…
```

**ROOT CAUSE:** the fast path hardcoded EIP-1559 fee shape:
```rust
effective = min(max_fee_per_gas, basefee + max_priority_fee_per_gas().unwrap_or_default())
```
A **LEGACY (type 0) transfer with no calldata is fast-path eligible**, but legacy has no priority
field, so `unwrap_or_default()` = 0 and the formula collapses to `min(gas_price, basefee)` =
**basefee**. The correct effective price for legacy is `gas_price` outright. The sender was
under-charged and the beneficiary (which Arc credits the FULL fee) under-credited. Gas stayed
21,000 and the log was unchanged, so **receipts matched perfectly and only the state root moved** —
invisible to the receipts digest added earlier that same day.

**FIX:** ask the transaction for its own price instead of assuming a shape —
`tx.effective_gas_price(Some(basefee))`. Correct for legacy, 2930 and 1559 by construction.

**GATE EXTENDED — `Workload::Mixed`:** 7,000 txs where every 7th carries calldata (forcing the
general-EVM path, which flushes the overlay and drops the caches) and every 5th of the rest is
LEGACY. Prints a post-state digest so the two gate runs can be compared directly. It reproduced
the bug immediately (`0xefd6…` stock vs `0x1769…` fast path) and matches after the fix. Note the
calldata/invalidation half alone did NOT reproduce anything — the overlay/cache invalidation is
fine; it was purely the fee shape.

**VALIDATED:** rebuilt, re-ran the same mixed live load — 200 consecutive blocks, 497,531 txs,
composition confirmed mixed (19,038 type-2, 1,032 type-0, 6,885 with calldata), all 4 identical on
stateRoot + blockHash + receiptsRoot + logsBloom, zero invalid blocks.

**PATTERN — three bugs, one shape.** Every fast-path bug so far came from *assuming* what a
transaction is instead of asking it: assumed no logs, assumed 1559 fees. The gate now covers
state + receipts + logs + legacy + general-EVM interleaving. Remaining unmodelled shapes that are
fast-path *eligible* and still uncovered: EIP-2930 with an empty access list, and zero-value
transfers. Worth adding before this flag is ever considered for default-on.

## 🚨 CONSENSUS BUG IN THE FAST PATH — FOUND, FIXED, AND THE "1.58x" WAS MOSTLY THE BUG (2026-08-08)

**`ARC_PARALLEL_TRANSFERS=1` was forking the chain against unmodified nodes.** Arc emits a log for
EVERY native value transfer (`ArcEvm::before_frame_init` -> `crate::log`). The fast path bypasses
the interpreter and emitted **none**, so its receipts and the block logs bloom differed from stock
while post-state was byte-identical.

Found by running the first-ever A/B against UNMODIFIED peers (val1 fast path, val2-4 stock). val1
rejected the first non-empty block outright:

```
Invalid block error on new payload number=100
validation_err=receipt root mismatch:
  got 0xe67e6405... (val1, fast path)  expected 0xafdca43b... (network, stock)
```

**Why every previous validation missed it — two independent blind spots:**
1. The offline gate compared post-STATE only. Receipts carry type/status/cumulative-gas/**logs**;
   all of those can differ with state intact.
2. Every live run enabled the fast path on ALL FOUR validators via the global `PAY_EL_ENV`. Four
   nodes running the same modified code agree with each other perfectly — and all four diverge
   from stock. **An optimisation must be A/B'd against unmodified peers, never against copies of
   itself.**

Beyond consensus this also silently dropped the `Transfer` events wallets and indexers consume.

**FIX** (`executor.rs`): emit the same log the interpreter would — EIP-7708 `Transfer` from Zero5
onward with self-transfers suppressed, legacy `NativeCoinTransferred` before that — reusing
`crate::log` and the same `is_arc_fork_active` gate so it is correct by construction.

**GATE STRENGTHENED**: `parallel_transfer_bench` now prints a receipts digest + log count. Before
the fix: stock `0x93a9…`/`0x9c0b…` with 47,618 logs vs fast path `0x21e2…` with 0 logs (and the
same digest for both workloads — the tell, since receipts should depend on the recipients). After:
digests match exactly. **Both gates must now agree on the receipts digest, not just state.**

**HONEST PERF — the fast path is worth ~3%, not 1.58x.** Once it correctly builds the 4,761 logs
per block, simultaneous same-block A/B (val1 fast path vs 3 stock):

| metric | stock | fast path | ratio |
|--------|-------|-----------|-------|
| exec (sub-metric) | 87.1 ms | 79.5 ms | 1.10x |
| per-tx exec | ~7.6 us | 3.77 us | 2.0x |
| **newPayload (the metric)** | **118.3 ms** | **115.0 ms** | **1.03x** |

The previously-claimed 1.39x-1.58x was substantially the cost of the logs it was not emitting.
Log construction (`encode_log_data`) is a large share of what revm was doing for a transfer. At
1.03x of newPayload — itself ~16% of a height — this is ~0.5% of block time. **Keep it gated OFF
by default; it is not worth deploying for 0.5%.**

**VALIDATED AFTER THE FIX:** 200 consecutive blocks / 952,200 txs, mixed config (val1 fast path vs
3 stock peers), all 4 identical on stateRoot + blockHash + receiptsRoot + **logsBloom**, zero
invalid-block errors.

This retroactively invalidates the "✅ MISSION COMPLETE — 1.58x, 120 heights zero divergence"
claim from mission 1: that run had the fast path on all 4 machines, so it proved self-consistency,
not correctness.

## ❌ EAGER RECOVERY WAS A MEASUREMENT ARTIFACT — REVERTED (2026-08-08)

**The previous iteration's "3.9x faster execution phase" was not real.** It was measured with
`transaction_wait` + `transaction_execution`, which do NOT sum to the cost of `newPayload`. Eager
recovery moved work OUT of the execution loop (into iterator construction) where those two
histograms cannot see it.

Caught by decomposing the height against an independent metric,
`reth_consensus_engine_beacon_new_payload_latency`. Same box, same 4,761-tx blocks, **simultaneous**
window, eager on val1 only, all four on `--engine.state-root-fallback`:

| val | newPayload | exec | root | other | wait/tx |
|-----|-----------|------|------|-------|---------|
| 1 (eager) | **85.9 ms** | 19.9 | 22.7 | **43.2** | 0.94 us |
| 2 | 87.5 ms | 58.2 | 25.1 | 4.1 | 8.72 us |
| 3 | 81.1 ms | 55.1 | 22.3 | 3.8 | 8.15 us |
| 4 | 81.7 ms | 55.4 | 22.6 | 3.7 | 8.50 us |

Execution fell 58.2 -> 19.9 ms (-38) while `other` rose 4.1 -> 43.2 ms (+39). **Total newPayload
was unchanged within noise** (85.9 vs 81.1-87.5). Exactly offsetting.

**WHY: the `wait` histogram is pipeline OVERLAP, not waste.** reth streams recovery concurrently
with execution, so the consumer's stall is time recovery is genuinely still working — overlapped
with useful work. Recovering eagerly converts that overlap into a serial barrier before execution
starts and returns precisely what it saves. `recovery_bench.rs` remains correct about the crypto
(33.4 us/tx serial, ~4 us/tx on 16 threads) — the wrong step was assuming the live gap was idle.

REVERTED in full (`crates/evm/src/evm.rs` back to plain delegation, rayon dep dropped). Kept: the
`recovery_bench.rs` and `recovery-probe.sh` harnesses, the `PAY_EL<i>_ENV` per-validator env hook
(genuinely useful for same-box A/B), and this record. Post-revert: both gates IDENTICAL, 200
consecutive blocks / 952,200 txs with all 4 validators agreeing.

**RULE ADDED TO THE PROTOCOL: an EL optimisation only counts if
`reth_consensus_engine_beacon_new_payload_latency` moves.** Sub-metrics can be relocated.

## ❌ Hypothesis #1 (engine-API ingestion) — REFUTED, and it was the top-ranked suspect

`experiments/dual-el/height-decomp.sh` (new) splits a height using reth's beacon-engine metrics.
At 100M gas / 4,761 txs, stock config, 715 ms height:

| phase | ms | % | who |
|-------|-----|---|-----|
| newPayload (EL busy) | 113.3 | 15.8 | exec 78.2 + root 29.3 + **other 5.8** |
| newPayload -> FCU (EL IDLE) | 342.5 | 47.9 | CL voting round |
| forkchoiceUpdated (EL busy) | 1.0 | 0.1 | EL |
| remainder (EL IDLE) | 258.6 | 36.2 | next proposer build + SSZ + streaming |

**EL busy 16%, EL idle 84%.** The "unaccounted" slice inside newPayload — the only place a hidden
JSON-decode/ingestion cost could live — is **5.8 ms**, ~0.8% of the height. There is no hidden
ingestion cost to recover, so **switching the payment lane to IPC cannot move cadence** and STEP 2
is closed. (CL<->EL is localhost in both the single-machine and fleet topologies, so transport
before reth's timer starts is bounded small too.)

This also independently reconfirms the 10/90 EL/non-EL split from a completely different metric
family than the earlier measurement.

## 🎯 Block-size sweep at fixed 2 blk/s — RE-RUN AND VALID (2026-08-08)

Harness fixed (all 3 bugs below), re-run on one chain with **all 4 payment ELs on the best-known
EL config** (`ARC_PARALLEL_TRANSFERS=1` + `ARC_EAGER_RECOVERY=1` + `--engine.state-root-fallback`),
runtime gas-limit flips, 4 local spammers, 75 s windows.

| gas | txs/blk | blk/s | ms/blk | tps | %full | exec | root | persist | |
|------|---------|-------|--------|------|-------|------|------|---------|--|
| 25M  | 1,190 | **1.99** | **504** | 2,363 | 100% | 3.8 | 10.5 | 49.0 | **HOLDS** |
| 50M  | 2,380 | 1.28 | 782 | 3,043 | 100% | 9.9 | 18.6 | 448.6 | degraded |
| 100M | 4,761 | 1.50 | 665 | **7,164** | 100% | 25.7 | 27.5 | 74.9 | degraded |
| 200M | 6,805 | 1.15 | 873 | 7,795 | 71% | 30.3 | 30.0 | 86.9 | degraded |
| 500M | 9,732 | 0.65 | 1532 | 6,353 | 41% | 39.3 | 31.7 | 802.8 | BROKEN |
| 1G   | 8,143 | 0.96 | 1043 | 7,810 | 17% | 39.0 | 33.7 | 111.8 | BROKEN |

**ANSWER TO THE MISSION GOAL: 2 blk/s is achievable at 25M gas — 1,190 tx/block, 504 ms, 2,363 tps.**
Everything larger trades latency for throughput, and the trade is set by consensus coordination, not
by the EL.

- **The EL is never the constraint at ANY block size.** Synchronous EL work (exec + root) is
  14.3 ms at 25M and only 72.7 ms at 1 Ggas — at most ~8% of block time, ~3% at the target. After
  eager recovery, execution at the 2 blk/s point is **3.8 ms/block**. There is no cadence left to
  win inside the EL; this closes the optimisation line the mission opened.
- **Throughput plateaus at ~7-8k tps** from 100M upward. Bigger blocks stop buying tps and only add
  latency — consistent with the earlier finding that the dominant per-height cost scales with
  TRANSACTION COUNT (SSZ encode + proposal streaming + voting), not with block count.
- **HONEST CAVEAT — the ≥200M rows are DELIVERY-bound, not chain-bound.** Blocks there are only
  71/41/17% full, so 4 local spammers could not offer enough load; those rows measure the spammer,
  not the lane. Only the 25M/50M/100M rows (100% full) are chain-limited. The fleet with
  distributed spam previously reached 9.5k avg / 15k peak at 1 Ggas.
- **The 50M row is noise**, not signal: persist spiked to 448.6 ms and its 1.28 blk/s is worse than
  100M's 1.50 at half the size. Disk stall on this box during that window. persist is the noisiest
  column throughout (49 → 803 ms, non-monotonic in block size).
- **Practical recommendation:** 25M for a latency demo (true 2 blk/s), 100M for a throughput demo
  (7.2k tps at 665 ms — 3x the tps for 33% more latency).

Consensus: 300 consecutive blocks / 1,582,375 txs across the whole sweep, **all 4 validators
identical on stateRoot + blockHash + receiptsRoot at every height**, with all four running eager
recovery (previously only val1 did).

### The 3 harness bugs (fixed in blocksize-sweep.sh; the original run produced a "3026% full" row)
1. polled a hardcoded val1 that had parked → read STALLED while the chain ran on 3-of-4. Now polls
   whichever validator has the highest head.
2. applied the gas-limit change *while saturating load ran*, so the governance tx was starved from
   the mempool and the limit silently never changed. Now load is STOPPED for the change and the new
   limit is VERIFIED on a fresh block header before measuring.
3. host map was fleet-only. Now defaults to the single-machine demo, `FLEET='{"1":"ip",...}'` for
   the 4-machine case.

## Block-size sweep at fixed 2 blk/s — ATTEMPTED, INVALID, 3 harness bugs found (2026-08-08)

Goal: hold latency at Arc mainnet's 500 ms and find the largest payment block the CL sustains.
`experiments/dual-el/blocksize-sweep.sh` sweeps the on-chain gas limit at runtime. **The run produced
numbers, and they are WRONG — do not quote them.** Tell-tale: "3026% full", i.e. gasUsed far above
the limit the row claimed to test. Every row actually measured 1 Ggas.

Three separate harness bugs, all now understood:
1. **Polled a hardcoded val1.** val1's CL parked, so the sweep read STALLED at every size while the
   chain ran fine on 3-of-4 quorum. FIXED: poll the highest-head validator.
2. **The governance tx never landed under saturating load.** `updateFeeParams` competes with ~47k
   spam txs that all pay the SAME fixed 20 gwei (our own fixed-fee design), so it is effectively
   FIFO behind a huge backlog and never gets included. => the sweep must set the gas limit with
   load OFF, then apply load, then measure — per size.
3. **`set-lane-economics.sh` targets `127.0.0.1`** = val1 only. With val1 parked, the controller tx
   went to a node not following the chain and sat there forever. => point governance txs at a
   HEALTHY validator (or heal val1 first); consider an env override for the RPC target.

NOTE this is a genuine operational finding, not just a test artifact: **on a saturated fixed-fee
lane, governance transactions cannot get in.** A fixed fee removes the fee market that would
normally let an urgent tx bid its way in. Worth a design note — an exempt/priority path for
controller txs, or admin submission via a reserved lane.

NEXT ITERATION should re-run the sweep as: for each size -> stop load -> set gas limit against a
healthy validator -> verify it took -> start load -> measure 70 s -> record.
