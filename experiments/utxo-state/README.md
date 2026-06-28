# utxo-state — payment-lane experiments (reproducible)

Standalone Rust crate (own `[workspace]`, no reth deps) behind the Arc payment-lane analysis.
All transactions are real secp256k1 (k256); state is in RAM; the commitment is an in-RAM binary
Merkle unless noted.

## Build & run
```bash
cd experiments/utxo-state
cargo build --release
./target/release/<bin> [args]
```

## Reproducibility notes (read this)
- **Determinism:** every bin uses a fixed-seed xorshift PRNG and (where signed) deterministic
  RFC-6979 ECDSA, so the *logical* result (UTXO sets, Merkle roots, conflict counts) is identical
  across runs. Roots are asserted equal where two methods compute the same state.
- **Timings vary ±~15% run-to-run** (CPU thermal/turbo, OS scheduling). **Parallel throughput is
  load-sensitive**: stop other heavy work (the local testnet, other sweeps) before parallel runs,
  or it drops ~10–15%. Report timings as approximate; the *qualitative* findings are stable.
- **Hardware here:** Intel i7-11700F (8C/16T), 62 GB RAM, NVMe. `k256` (pure Rust) is ~2× slower
  than `libsecp256k1`; treat absolute tx/s as a conservative floor.
- **No extrapolation:** every number is measured over the stated range; we do not project beyond it.

## Bins, commands, and what to expect (approximate, this box)
| bin | command | measures | expected (approx) |
|---|---|---|---|
| `utxo-bench` | `utxo-bench 25000000 5000000` | RAM + 3 commitments vs set size | 25M ≈ 3.4 GB; ECMH commit ~0.22–0.26 s/blk **flat**; MPT/Merkle O(n) |
| `merkle_acc` | `merkle_acc 25 25000000` | hash-only Merkle vs size | 25M ≈ 5.5 GB; ~0.26–0.33 s/blk **flat**; ~9–11 µs/update |
| `utxo_tx` | `utxo_tx` | signed UTXO tx end-to-end | single ~9–11k tx/s; verify ≈ 73–88 µs ≈ 75% of cost |
| `compare` | `compare` | UTXO vs account, single-thread | ~equal (~10–11k tx/s); account marginally faster |
| `contention` | `contention` | account contention, real RMW | hot: 97% conflict, max acct deg ~1309; tx/s unchanged (execute ≈ 0 µs) |
| `parallel_cmp` | `parallel_cmp` | parallel throughput | both ~90–105k tx/s (idle), ~8–9× scaling |
| `locality` | `locality` | dense vs hash-keyed store | dense beats sparse 1.1×→1.3–1.4×, gap widens with size |
| `two_el` | `two_el` | two-EL/two-root, parallel | block time = max(T_evm, T_pay); lane ~free below crossover (~10k pay/blk) |

## Honest findings (see ../../CLAUDE.md for the full record)
- For **simple payments, UTXO ≈ account** (both signature-verification-bound). UTXO is not faster
  or smaller.
- **Account contention is real but invisible** for simple transfers (the contended work — a balance
  add — is ~0 µs vs ~9 µs/tx parallel verify). Caveat: a real Block-STM also pays scheduler/abort
  overhead this does not model; UTXO avoids that machinery (deterministic, zero aborts).
- **The win is isolation:** a payment-only state grows with users (not tx volume), fits in RAM, and
  escapes reth's shared disk-bound MPT. Reusing reth (account + Block-STM) is the pragmatic design.
- **A locality-keyed, unified state+commitment structure** is the open refinement (1.1–1.4× in RAM
  here; larger on disk). This is the research front: the commitment for the lean EVM state.
- Known bug we fixed: `parallel_cmp`'s first account path wrote a constant leaf and did not model
  RMW/contention; `contention` is the corrected test. Also fixed a parallel-Merkle early-`break`.
