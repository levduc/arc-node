# MISSION: parallel payment lane, running and proven

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
- [ ] **NEXT: runtime-verify the in-node fast path.** Suggested cheapest check: run the
      single-machine demo twice under identical load — once stock, once with
      `PAY_EL_EXTRA_ARGS` carrying the env var — and confirm the payment lane still advances with
      all validators agreeing (any semantic error diverges the root and halts consensus). Then
      compare `exec_ms` (baseline ~305ms at full 1 Ggas). NOTE: env must reach the container —
      `launch-payment-els.sh` passes ARGS, not env; add `-e ARC_PARALLEL_TRANSFERS=1` to the
      `docker run` there (and in fleet/demo-fleet-metamask.sh `payment_el_cmd`).
- [ ] (old) implement `run_fastpath` semantics in `crates/evm/src/executor.rs` (all of it in arc-evm; ~150–250 lines):
      buffer plain transfers during validation → execute fast/parallel at `finish()` → feed the
      existing `commit_transaction()` so receipts/gas/bloom stay production code. Env-gated
      (e.g. `ARC_PARALLEL_TRANSFERS=1`) so only the payment EL opts in.
      Transfer gate must require: empty input, empty access list, no authorization list,
      `TxKind::Call`, and `to` has empty code. Gas used = 21 000. Fee: `effective_gas_price =
      min(max_fee, basefee + max_priority)`; sender pays `21000 * effective`, beneficiary is
      credited the SAME full amount (Arc does not burn the base fee).
- [ ] Add a differential test for the fast path itself (fast path vs real EVM), same shape as the
      parallel bench.
- [ ] `make build-docker` → single-machine 4-validator demo → confirm 100–1000 blocks of agreement.
- [ ] Ship image to the 3 remotes (`docker save | gzip | tailscale ssh docker load`) → fleet run →
      confirm again across machines.
- [ ] Only after all green: consider dropping the state root entirely (frozen root), re-verify.

## Rules for each iteration
- Small, verified increments. Run the differential bench before any deploy.
- Never leave the repo uncommitted-broken or a chain half-deployed. `apply-fork.sh revert` before
  any Docker build (the fork is no longer needed at all).
- If blocked, write the finding into this file + `CLAUDE.md` so the next iteration starts informed.
