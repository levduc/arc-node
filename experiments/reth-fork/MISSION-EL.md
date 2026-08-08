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
- [ ] **STEP 1 (NEW TOP PRIORITY): BATCH EXECUTION inside ArcBlockExecutor.** This is the unlock
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
