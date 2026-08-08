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
