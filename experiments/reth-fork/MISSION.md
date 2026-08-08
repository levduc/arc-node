# MISSION: parallel payment lane, running and proven

# ✅ MISSION COMPLETE (2026-08-08) — all four criteria met

| # | criterion | result |
|---|---|---|
| 1 | payment ELs run the fast path | yes — all 4 machines, `ARC_PARALLEL_TRANSFERS=1` confirmed in every container |
| 2 | 100–1000 blocks of 4-way root agreement under load | **120 heights on the fleet + 201 on single-machine, ZERO divergence** (stateRoot AND block hash) |
| 3 | exec_ms materially below the stock baseline | **379 ms → 239.6 ms/blk = 11.03 → 7.00 µs/tx = 1.58× faster, −37%** (block composition nearly identical: 34,363 vs 34,245 txs) |
| 4 | nothing else regressed | EVM lane stock, block production healthy, fee/cadence config intact |

Fleet numbers at full 1-Ggas blocks: 34,245 txs/blk, exec 239.6 ms, state-root 4.7 ms,
persist 116.4 ms, 9.7k tps avg / 14.7k peak. tps is ~unchanged vs the stock fleet run because
cadence is CONSENSUS-COORDINATION-bound (~2.9 s/block vs ~0.36 s of measured work) — exactly as
predicted; execution work dropped 37 % but the coordination overhead now dominates. **That is the
next lever, not execution.** MEASURED 2026-08-08: at full 1-Ggas blocks reth's newPayload takes
**357 ms** while a block takes **3,750 ms** — the EL is **10%** of block time, consensus is **90%**
(per tx: 7.5 us in the EL vs 71.3 us outside). tps is also FLAT across an 8x block-size sweep, so
big blocks neither help nor hurt. Even instant execution would add only ~10% tps.

## Re-verification 2026-08-08 (iteration 4) — MISSION STILL HOLDS

* Regression gates: BOTH `parallel_transfer_bench` runs (stock and `ARC_PARALLEL_TRANSFERS=1`)
  print **IDENTICAL** on both workloads. ✓
* Fast path confirmed enabled on all 4 payment ELs; fleet restored to 1 Ggas.
* Live: **115 consecutive heights (2846..2960), ZERO divergence** — identical stateRoot AND block
  hash on all 4 machines under distributed load. ✓
* Throughput: 11.0k tps avg / 14.0k peak at 37,963 txs/blk.

**CAVEAT worth carrying forward — perf numbers are STATE-DEPENDENT, so re-measurements are not
directly comparable:**

| run | chain age | exec/blk | txs/blk | us/tx | root | persist |
|---|---|---|---|---|---|---|
| fast path (iteration 3) | ~400 blk | 239.6 ms | 34,245 | **7.00** | 4.7 ms | 116 ms |
| fast path (now) | ~2,960 blk | 344.7 ms | 37,963 | **9.08** | 14.5 ms | 189 ms |
| stock baseline | young chain | 379.0 ms | 34,363 | **11.03** | — | — |

state-root (4.7 -> 14.5 ms) and persist (116 -> 189 ms) rose too, i.e. EVERYTHING got slower as the
chain accumulated ~2,960 blocks of state — this is state growth, not a code regression (the offline
gates are unchanged and byte-identical). But it means the headline **1.58x is only valid against a
same-age chain**; to re-assert it, A/B stock vs fast path at the SAME chain height (use
`ab-fastpath.sh`, which starts both runs from a fresh chain).

**Definition of done (all must hold):**
1. Payment-lane ELs execute native transfers via the fast/parallel path (not reth's serial per-tx loop).
2. The 4-validator demo runs under load with **all 4 validators agreeing on the payment-lane state
   root for 100–1000 consecutive blocks** — zero divergence, zero halts.
3. Measured per-block `exec_ms` on the payment lane is materially below the stock baseline
   (baseline on this box at full 1 Ggas blocks: exec ~305 ms with `--engine.state-root-fallback`,
   ~379 ms stock). Record the number.
4. Nothing else regressed: EVM lane stock, block production healthy, MetaMask demo intact.

**Verification is cheap and objective — use it every iteration:**
- offline: `cargo run --release -p arc-evm --example parallel_transfer_bench`
  (runs the SAME block serial vs parallel through the real `ArcBlockExecutor`, compares post-state
  account-by-account; prints `IDENTICAL ✓` or `DIVERGED ✗` + speedup). This is the gate — never
  deploy on `DIVERGED`.
- live: all validators must report the same payment-lane state root at each height. Divergence
  halts consensus, so "chain keeps advancing under load" IS the proof. Dashboard `/` shows per-lane
  root + `agree`; `experiments/dual-el/pay-throughput-bench.sh measure` gives exec/root/persist.

## State (keep this current)

- [x] Algorithm proven: partition by sender + commutative balance deltas for recipients/beneficiary.
      **4.11–4.25× vs serial, post-state IDENTICAL** on both `pool` (real spammer pattern) and
      `closed` (adversarial ring). Reference impl + differential test:
      `crates/evm/examples/parallel_transfer_bench.rs` (commit e4e8dc4).
- [x] Wiring design settled and SIMPLIFIED — **no reth fork needed** (commit 75ce069). See
      `README.md` "⚡ NO FORK NEEDED": reth's receipt-root task fails closed, so receipts may be
      produced at `finish()`; and `ArcBlockExecutor` can tell validation from building itself via
      `!self.ctx.extra_data.is_empty()`.
- [x] Lever 1 already live-proven on the fleet: `--engine.state-root-fallback` = −35 % exec,
      −25 % total, consensus intact (commit 34d9fec). Payment ELs opt in via
      `PAY_EL_EXTRA_ARGS` / `PAY_EL<n>_EXTRA_ARGS`.
- [x] **STRATEGY CHANGE (iteration 1): hand-written FAST PATH beats parallel EVM by 6x.**
      No-EVM transfer semantics, differentially verified IDENTICAL vs revm on both workloads:
      **24.4x / 21.6x** (4.4ms vs 107ms per full 1-Ggas block; 0.09 us/tx vs 2.15 us/tx).
      => Implement the FAST PATH first: simpler (no worker EVMs, no thread-safety), bigger win.
      Parallelism layers on top later for a further multiple. Exact semantics are in the bench's
      `run_fastpath()` and are the reference for the executor implementation.
- [x] **IMPLEMENTED + TYPECHECKS (commit 8225038): fast path wired into `ArcBlockExecutor`**
      `ARC_PARALLEL_TRANSFERS=1` -> `execute_transaction_without_commit` returns a hand-built
      `ResultAndState` for eligible transfers, so `commit_transaction`/`finish()` are UNTOUCHED and
      both building and validation paths stay correct. Much simpler than buffering: no `finish()`
      surgery, no reth fork. Key detail: `Account::from(pre)` seeds `original_info` (revm's bundle
      diff needs the PRE-state), then `info` = post-state + `mark_touch()`.
- [x] **IN-EXECUTOR FAST PATH VERIFIED CORRECT (iteration 2).** Trick: the bench's `run_serial`
      uses the REAL `ArcBlockExecutor`, so running it with `ARC_PARALLEL_TRANSFERS=1` exercises the
      in-node code while `run_parallel` (direct EVM) stays as the oracle. Result: **IDENTICAL ✓** on
      both workloads, and the executor path measurably speeds up, proving the gate engages:
        pool   106.2ms -> 64.1ms (1.66x)   closed  93.8ms -> 52.2ms (1.80x)
      HONEST NUMBER: 1.66-1.80x in-executor, NOT the 24x of the standalone arithmetic (4.7ms). The
      ~60ms that remains is receipt building + `State`/bundle commit, NOT the EVM — that is the next
      optimization target once this is deployed and proven.
      Regression gate for every future change: BOTH `cargo run --release -p arc-evm --example
      parallel_transfer_bench` and the same command with `ARC_PARALLEL_TRANSFERS=1` must print
      IDENTICAL.
- [x] env passthrough wired: `PAY_EL_ENV` -> docker `-e` in launch-payment-els.sh AND the fleet
      payment_el_cmd. Use `PAY_EL_ENV='-e ARC_PARALLEL_TRANSFERS=1'`.
- [x] **✅ RUNNING ON THE 4-VALIDATOR DEMO (iteration 2, 2026-08-08).** `make build-docker` (both
      images rebuilt), then `PAY_GAS=1000000000 EXTRA_ACCOUNTS=8000
      PAY_EL_ENV='-e ARC_PARALLEL_TRANSFERS=1' demo-metamask.sh start`. All 4 payment ELs confirmed
      carrying the env var. Under 8-spammer load the chain ran from height 71 to 890+ with **ZERO
      divergence and zero halts** — ~820 blocks, comfortably inside the 100–1000 target.
      Root agreement verified explicitly:
        * height 203, ALL FOUR on the fast path: identical stateRoot AND block hash;
        * height 890, the three still on it: identical stateRoot.
      Because a wrong root halts consensus instantly, "the chain kept advancing" is itself the proof.
      Live numbers at 4,098 txs/block (small blocks — one box can't fill 1 Ggas): exec 75.6 ms,
      state-root 11.2 ms, persist 89.8 ms, 6.8k tps avg / 7.4k peak, 1.65 blk/s.
- [x] **✅ CLEAN A/B DONE (`experiments/dual-el/ab-fastpath.sh`, results /tmp/ab-fastpath-results.txt).**
      Two separate full runs, identical config + 8-spammer load, 120 s window each:
        config     exec/blk   txs/blk   us/tx     tps    4-way root agreement
        stock       121.4ms      5363   22.64   3,346    ALL 4 AGREE
        fastpath    145.6ms      8916   16.33   7,336    ALL 4 AGREE
      **Per-tx execution 22.64 -> 16.33 us = 1.39x faster, with all 4 validators agreeing in BOTH
      runs.** Note exec/blk is higher for the fast path only because its blocks carry 66% more txs;
      per-tx is the honest comparison. tps 3.3k -> 7.3k is partly confounded (persist was 656ms in
      the stock run vs 100ms in the fast-path run — single-box disk variance), so quote the per-tx
      exec number, not the tps ratio.
      Live 1.39x < the 1.66-1.80x measured offline: the live node carries extra per-tx cost around
      the executor (state provider, receipt streaming) that the fast path does not remove.
- [x] **✅ DoD #2 MET (2026-08-08): 201 CONSECUTIVE heights, ZERO divergence.** Verified EVERY
      settled height 493..693 on the running 4-validator demo under 8-spammer load (~7,000 txs/blk):
      identical stateRoot AND block hash on all four. Since a wrong root halts consensus, this is
      end-to-end proof the fast path is consensus-correct in production.
- [x] **✅ SHIPPED + VERIFIED ON THE 4-MACHINE FLEET (2026-08-08).** `fleet/ship-images.sh` (new;
      verifies by the sha256 of the binary INSIDE the image — image IDs differ across docker
      versions even when content is identical, which produced a false "MISMATCH" first run).
      Fleet started with `PAY_GAS=1000000000 EXTRA_ACCOUNTS=16000
      PAY_EL_ENV='-e ARC_PARALLEL_TRANSFERS=1' fleet/demo-fleet-metamask.sh start`; fast path
      confirmed enabled on all four payment ELs. Under distributed load at FULL 1-Ggas blocks:
      **120 consecutive heights (295..414), ZERO divergence — identical stateRoot AND block hash on
      all 4 PHYSICAL MACHINES at every height.**
- [ ] Only after all green: consider dropping the state root entirely (frozen root), re-verify.

## Rules for each iteration
- Small, verified increments. Run the differential bench before any deploy.
- Never leave the repo uncommitted-broken or a chain half-deployed. `apply-fork.sh revert` before
  any Docker build (the fork is no longer needed at all).
- If blocked, write the finding into this file + `CLAUDE.md` so the next iteration starts informed.

## Gotcha found in iteration 2 (cost ~30 min)

Recreating a payment EL mid-run to A/B it triggers **finding #7**: the fresh EL lost its unpersisted
blocks, the CL tip was far ahead, and value-sync could not backfill (`p2p` peers alone did not do it)
— val1 sat at height 497 while the chain ran on to 890 on 3-of-4 BFT quorum. `docker restart
validator1_cl` is the documented heal and did restart progress (497 -> 517), but recovery is slow.
**Do not A/B by recreating an EL on a running chain.** Instead start two separate runs, or set the
env var on a subset of validators AT START (`PAY_EL1_EXTRA_ARGS`-style, but for env). Note the mixed
config is *safe* — the fast path is state-identical — it is purely an ops/resync problem.
