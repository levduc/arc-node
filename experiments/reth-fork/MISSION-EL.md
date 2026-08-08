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
- [ ] **NEW TOP PRIORITY: cut receipt + commit overhead (78% of what remains).** Hypothesis: the
      cost is `db.commit(state)` per transaction on `State<DB>` with bundle tracking — 3 accounts x
      47,618 txs = ~143k TransitionAccount records, each with allocations. Idea: keep a per-block
      overlay of pending account changes in the executor, serve fast-path reads from it, and commit
      ONCE at `finish()` — ~16k transitions instead of 143k. Receipts stay per-tx (they are cheap
      and must keep cumulative-gas order).
      RISK TO SETTLE FIRST (measure before writing): committing once at the end changes how
      BundleState records reverts/original_info. The final PLAIN state (hence the state root) should
      be identical, but revert data matters for reorgs — verify with the gates AND by inspecting the
      bundle, not just balances. Also confirm nothing between txs reads committed state directly.
      CHEAP PRE-CHECK: time `db.commit()` alone vs receipt building alone, to confirm which half of
      the 42 ms dominates before touching anything.
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
