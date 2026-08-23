# CLAUDE.md — arc-node working notes

Arc is an EVM-compatible L1: a **customized Reth** execution layer (built on the Reth SDK, git tag `v1.11.3`) driven by the **Malachite** BFT consensus layer over the standard Ethereum **Engine API** (IPC/RPC). See `README.md`, `crates/node/README.md`, `crates/malachite-app/README.md`.

## Toolchain (and gotchas that cost real time)

- **Node.js ≥ 20.19 / 22.x (even versions).** Node 18 breaks the Hardhat genesis step with a misleading `HH19` error — the real cause is `ERR_REQUIRE_ESM` on the ESM-only `@noble/ed25519`. CI uses Node 24. Install via nvm; do not use system Node 18.
- **Rust** per `rust-toolchain.toml` (currently `1.91.1`), via rustup.
- **clang + libclang-dev** are required on the host to build `quake`/the node — `reth-mdbx-sys`'s bindgen needs clang's builtin headers (`stdarg.h`). Missing them gives `failed to run custom build command for reth-mdbx-sys`.
- **Docker** for `make testnet` (builds CL/EL images, runs containers via `quake`).
- **Foundry** pinned in `.foundry-version`.

## Common commands

```bash
make build                 # cargo build the node
make genesis               # generate assets/localdev/genesis.json (idempotent)
make testnet               # genesis + build-docker + quake start (5 validators + 5 CL + monitoring)
make testnet-down          # stop testnet
make testnet-clean         # remove testnet artifacts
make testnet-load RATE=1000 TIME=60   # drive load (wraps quake load)
make test-unit             # rust unit tests + lint
```

`quake` is the in-repo testnet manager (`crates/quake`), run as `cargo run --bin quake -- <args>` (the Makefile wraps this). It is NOT a global binary. Node RPC endpoints for a running local testnet are in `.quake/localdev/nodes.json` (validator1 EL: http `8545`, ws `8546`).

## Execution-throughput benchmarking

Two tools measure execution-layer performance:

- **`arc-engine-bench`** (`crates/engine-bench`) — replays real Arc blocks into a fresh node via `engine_newPayloadV4` + `engine_forkchoiceUpdatedV3` and reports per-block + aggregate import throughput. This is the apples-to-apples way to compare execution clients (no consensus gating).
- **`spammer`** (`crates/spammer`) / `quake load` — generates transaction load. ERC20 transfer load: `spammer ws --targets ws://127.0.0.1:8546 -r 4000 -g 4 -t 60 --mix erc20=100`.

A end-to-end baseline harness lives at **`scripts/bench/engine-baseline.sh`**. Workflow: drive load into a running testnet → `prepare-payload --from 1 --to <head>` → replay into a fresh target node booted with the **testnet's own genesis** (`.quake/localdev/assets/genesis.json`) → read results.

Benchmarking gotchas:
- The replay source must be **real consensus blocks**, not a reth `--dev` node — engine-bench assumes Arc's convention `parent_beacon_block_root = parent execution block hash` (`crates/eth-engine/src/engine.rs`), which dev blocks violate (→ "block hash mismatch").
- The target node's genesis must exactly match the source testnet's, or you get an "expected parent block 0" mismatch.
- Run bench nodes on non-testnet ports (e.g. http `7545`, authrpc `7552`, p2p `30402`); never `pkill -f "arc-node-execution node"` (it matches the testnet containers).
- `summary.csv` averages are diluted by empty blocks — filter `combined_latency.csv` to `gas_used > 0` for the real number.

### Key finding (ERC20 workload)

For ERC20-transfer block import, the per-stage cost split (from Reth's `reth_tree_root_*` / `reth_sync_execution_*` / persistence metrics) is roughly: **EVM execution ~4%, trie/state-root ~58%, disk persistence ~38%.** EVM execution is not the bottleneck — state-root/merklization dominates. Implication for alternative execution clients (e.g. Galxe's gravity-reth): a faster *parallel EVM* (grevm) targets the ~4% slice; *parallel merklization* targets the ~58% slice.

Validated on **real testnet state** (169 GB snapshot, block 48.75M): a 2.7M-gas block imports in **27.5 ms (97 Mgas/s)** vs ~1,900 Mgas/s on shallow state — **~20× slower**, with merkle/state-root **27.8 ms** and persistence **24.5 ms** dominating, EVM execution only **4 ms**. A warm re-replay (trie pages cached) halves it to 11.3 ms (235 Mgas/s), proving ~60% of merkle is cold disk-I/O. This is exactly what **reth 2.0's Sparse Trie Cache + Partial Proofs** target (published ~2 ms state-root), making a reth-2.0 upgrade the highest-value execution lever for Arc.

## reth 2.0 upgrade — attempt log

Goal: port Arc's execution layer from reth 1.11.3 → reth 2.0 (the upstream, maintained fix for the state-root/I-O bottleneck above). This is a multi-week port, NOT a dependency bump. Status: **LAYER 1 COMPLETE — `cargo generate-lockfile` resolves clean (exit 0, 0 errors, 103 reth crates at v2.0.0). Compilation not yet attempted (blocked on layer 2).** Branch: `reth-2.0-layer1`.

How layer 1 was resolved: repointed 42 `reth-*` deps to `tag = "v2.0.0"`; aliased the removed crate via `reth-primitives-traits = { ..., package = "reth-ethereum-primitives" }` (placeholder so the lockfile resolves — the real per-type relocation is a layer-2 task); bumped `rust-toolchain.toml` 1.91.1 → 1.93.0. **The resolve immediately surfaced the layer-2 wall: the lockfile now contains BOTH revm 34 (Arc's own EVM crates) AND revm 36 (reth 2.0) — they don't unify.** Arc's revm sub-crate pins are revm-34-era (`revm-interpreter = 32`, `revm-primitives = 22`, `revm-context-interface = 14`); reth 2.0 needs the revm-36-era set. So step 1 of layer 2 is bumping ALL `revm-*` workspace pins to reth-2.0's exact versions, then porting Arc's EVM code to the new revm API.

The 5 layers of the port (in order):
1. **Crate-source reconciliation** — reth crates are git-only (NOT on crates.io; verified). Of Arc's 42 `reth-*` deps, **40/42 exist at git tag `v2.0.0`**; only **`reth-primitives-traits` is removed** (primitives reorganized under `crates/ethereum/primitives`, `crates/payload/primitives`, `crates/engine/primitives`; its traits likely moved to alloy / `reth-ethereum-primitives` — TODO: find new import paths). Earlier mechanical `sed v1.11.3→v2.0.0` failed *only* on that one dangling crate. Scope of the relocation is small: Arc imports just **~8 types** from `reth_primitives_traits` across **~16 sites** (`SignerRecoverable`, `RecoveredBlock`, `SealedHeader`, `NodePrimitives`, `SealedBlock`, `Recovered`, `Block`, `InvalidTransactionError`) — `grep -rn reth_primitives_traits:: crates/` to find them; repoint to their reth-2.0 home.
2. **revm 34 → 36** — Arc's `crates/evm` (handler.rs, evm.rs, opcode.rs, log.rs, precompiles) is written against revm 34's `Handler`/`Context`/`Evm` API; revm 36 has breaking changes. Bulk of the work #1.
3. **reth SDK 1.11 → 2.0** — `ConfigureEvm`, `BlockExecutorFactory` (alloy-evm), node builder, payload primitives, and **Storage V2** APIs changed; port `crates/node`, `crates/evm-node`, `crates/execution-config`. Bulk of the work #2.
4. **Toolchain** — rust 1.91.1 → 1.93 (reth 2.0 `rust-version`), bump alloy 1.6.3 → ~1.8.2 (resolves up automatically as same-major).
5. **Datadir migration** — existing nodes' MDBX data is Storage-V1; reth 2.0 defaults to Storage V2 (`storage_v2: true`), so a re-sync/migration from snapshot is required.

Reference clone at `/home/papaduck/reth-2.0-ref` (git tag v2.0.0). Crate name list: `/tmp/reth20_names.txt`.

### PROGRESS LOG (most recent first)
- **🎯 LIVE 4-NODE reth-2.3 + MALACHITE CONSENSUS TESTNET — WORKS END-TO-END (2026-06-26).** Both binaries compile on reth-2.3 (EL arc-node-execution + CL arc-node-consensus; CL needed only PayloadAttributes.slot_number since CL↔EL is just the Engine API). Built reth-2.3 Docker images (Dockerfile base rust 1.91→1.93). Also bumped workspace reqwest 0.12→0.13 (alloy-2.0 pulls 0.13) + renamed reqwest TLS features (rustls-tls→rustls, rustls-tls-native-roots→rustls, add json feature) so quake/arc-checks build. Ran `quake -f crates/quake/scenarios/localdev4.toml start` (new 4-validator manifest). RESULT: 4 validators (CL+EL) produce blocks via Malachite BFT (~2 blocks/s), reth-2.3 EL executes with storage_v2=true on FRESH chain (no migration). ERC20 spam (45s): 73,595 txs ALL landed in 88 blocks, many FULL at 30M gas / 868 ERC20 transfers (0xa9059cbb), state-root ~277µs shallow. → reth-2.3+Malachite operates as a normal consensus node. GOTCHA: `cast block` in a tight loop times out on a busy node — use direct eth_getBlockByNumber RPC. TODO: "join public Arc testnet" (needs public CL bootnodes/chainspec + V1→V2 migration or genesis sync + live P2P).
- **reth 2.3 PORT COMPLETE — full binary typechecks (2026-06-26), branch `reth-2.3-upgrade`.** `cargo check -p arc-node-execution` = 0 errors against reth 2.3 / revm 40 / alloy 2.0.5 / ssz 0.10. Deltas beyond reth-2.0: arc-precompiles (revm-40 redesign: PrecompileError=Fatal-only, failures→PrecompileOutput.status Revert/Halt, new(g,b,0)/revert()/halt(PrecompileHalt::OutOfGas)); arc-evm (BlockExecutor new Result/TxResult, commit_transaction→GasOutput, set_state_hook REMOVED→reth installs at DB layer via State::set_state_hook so Arc's on_state forwarders were redundant, SELFDESTRUCT opcode→revm-40 Err-return contract, known_bytecode now (B256,Bytecode) executed directly, Send bounds, create_executor sig); arc-consensus-types (ssz 0.9→0.10 to match alloy 2.0 ExecutionPayloadV3 Encode); arc-execution-validation (ConsensusError::Other→msg(), validate_block_post_execution 5-arg+whole-result); arc-execution-payload (parent_block_info, slot_number=None, mark_invalid by-value, gas_used.tx_gas_used(), EthBuiltPayload::new 4-arg=Arc::new(block)+None); arc-evm-node (RpcAddOns::new 6-arg, slot_number). Building /tmp/arc-reth23. NOT VALIDATED: consensus-correctness (esp. SELFDESTRUCT opcode + known_bytecode) needs differential replay. Ref: /home/papaduck/reth-2.3-ref, cargo checkout 9384bc5.
- **🎯 STORAGE-V2 WIN MEASURED ON REAL ARC STATE (2026-06-26).** Migrated the 169GB V1 snapshot → V2 via `arc-node-execution db migrate-v2` (44min: changesets→static files, history/tx-hash→RocksDB, MDBX 142→58GB, flip storage_v2=true), then merkle rebuild on boot (~12min, MerkleExecute re-merklizes from hashed state). GOTCHAS (pruned snapshot): migrate-v2 reset SenderRecovery to 0 but txs only exist from 48.5M (pruned) — set checkpoint via `db stage-checkpoints set --stage sender-recovery`; the TransactionSenders static file is empty/buggy (reth-2.3 migrate-v2 + pruned bug: "append as block #0 but expected 48500000") so LIVE persistence of new blocks crashes — WORKAROUND: SenderRecovery=tip + boot with `--engine.persistence-threshold 20000 --engine.memory-block-buffer-target 20000` so blocks import IN-MEMORY (state-root computed pre-persistence). RESULT replaying 300 real blocks (48835032+) into reth-2.3 V2: **state-root 2.44ms avg (vs V1 cold 27.8ms = 11.4x, vs V1 warm 11.3ms = 4.6x)**; block import newPayload 4.89ms (vs V1 cold 27.5ms = 5.6x, vs legacy-V1 warm 9.7ms = 2x); 373 Mgas/s. Matches reth's published ~2ms state-root target ON REAL ARC STATE. Replay imported all 300 with correct hashes → also consensus-validates the reth-2.3 port + the migration. CAVEAT: V2 ran in-memory (persistence buffered due to senders bug) — newPayload/state-root are the synchronous compute (persistence is async anyway) so comparison is sound for execution+state-root; full on-disk persistence in V2 blocked by the senders static-file bug on this pruned snapshot.
- **reth-2.0 port VALIDATED + head-to-head MEASURED (2026-06-26).** Release binaries: reth-1.11 `/tmp/arc-reth1`, reth-2.0 `/tmp/arc-reth2`, engine-bench `/tmp/bench-reth1`. 176-block loaded-ERC20 fixture `/tmp/exp-fixture` (genesis `/tmp/exp-genesis.json`, block0 0xc7e3c695...). Replayed SAME fixture into both (isolated). reth-2.0 accepted all 176 blocks with IDENTICAL hashes/state-roots → **differential replay validated the port is consensus-correct** (storage_v2=true). Throughput IDENTICAL on shallow state: reth-1.11 2154 Mgas/s, reth-2.0 2129 Mgas/s. EXPECTED — reth-2.x's Sparse-Trie-Cache win only appears when state EXCEEDS RAM (62GB); a fresh chain can't reach it. Win already measured via warm/cold on real 169GB: 27.8ms cold vs 11.3ms warm = 2.5x.
- **reth 2.3 bump (branch `reth-2.3-upgrade`): deps resolve, arc-evm has 27 revm-40 errors.** 2.3 = revm 36→40 + alloy 1.6→2.0.5 + reth-primitives-traits 0.4.1. GOTCHA: pin alloy `~2.0.5` (2.1.0 adds EthPayloadAttributes.target_gas_limit, breaks reth's reth-engine-local); revm-statetest-types=19.0.3. arc-evm errors are revm-40 precompile API: `PrecompileOutput::new_reverted`/`reverted` gone, `PrecompileError::{OutOfGas,Other}` renamed — find new names in revm-precompile 36.x. Grindable batch, plus alloy-2.0 changes still to surface. NOTE: 2.3 also won't change the shallow benchmark.
- **STRATEGIC: to see the win on Arc needs LARGE (RAM-exceeding >62GB) state.** Rebuild-from-genesis = TOO MUCH (~48M public-testnet blocks, days). BEST PATH (confirmed feasible-ish): reth 2.x AUTO-DETECTS storage version from existing datadir metadata (reth source `crates/storage/db-common/src/init.rs:170` reads stored settings, defaults v1; `EnvironmentArgs::storage_settings` honors `--storage.v2` for NEW dbs only) and supports `StorageSettings::v1()` legacy mode. So the reth-2.0 binary can likely OPEN the reth-1.11 V1 169GB snapshot in legacy mode — gets the in-memory Sparse-Trie-Cache win (~2.5x, the main one) but NOT the on-disk Storage-V2 win (needs migration). Unknown: reth-2.x legacy on-disk table compat with reth-1.11 exact format. TEST: `arc-snapshots download --chain arc-testnet` (~1-2h, EL 169GB), then `/tmp/arc-reth2 node --chain arc-testnet --datadir <snapshot> [--storage.v2 false]`, then engine-bench replay real blocks 48.75M+ → measure vs reth-1.11 baseline (27.8ms cold). The reth-2.0 binary is consensus-correct so it CAN execute Arc blocks.
- **🎯 FULL `arc-node-execution` BINARY BUILDS + RUNS ON reth 2.0** (`cargo build --bin arc-node-execution` → 2.4GB binary; `--version`/`node --help` work). All crates ported: arc-evm, arc-execution-payload, arc-evm-node, and the rest (execution-config/txpool/validation, types, node) compiled with the recurring fixes. Recurring patterns: (1) `EthPayloadBuilderAttributes`→`EthPayloadAttributes` + drop `PayloadTypes::PayloadBuilderAttributes` assoc type; payload_id moved attributes→PayloadConfig; BuildArguments gained execution_cache/trie_handle (sparse-trie cache, passed None); `BlockBuilder::finish(sp, None)`; `EthBuiltPayload::new` dropped id; attributes method→field access. (2) `TaskExecutor` is now a type alias for `reth_tasks::Runtime` (drop Box). (3) `builder.extra_data` is `Bytes` → use `default_extra_data_bytes()`.
  **NOT YET VALIDATED:** compiles + runs ≠ consensus-correct. Unverified: differential block/state-root output; the `set_state_clear_flag`-removal and sparse-trie-cache-as-None choices need Arc's differential/e2e suite. Also: reth 2.0 uses **Storage V2** so it CANNOT read the old reth-1.11 169GB Arc snapshot — running it needs a FRESH genesis/chain. So a real reth-2.0-on-Arc number is now *reachable* (fresh chain + load + engine-bench), but NOT on the existing snapshot, and only trustworthy after differential validation.
- **`arc-evm` COMPILES against reth 2.0 / revm 36 / alloy-evm 0.30 (10 errors → 0).** This is the core/hardest crate (all EVM customizations). Fixes: `TxResult::into_result` added; revm-36 `Bytecode` struct (`eip7702_address()`/`new_eip7702()`); state-clear now unconditional; and the big one — alloy-evm 0.30 **DB model**: executor DB is bounded by `StateDB` (Database+DatabaseCommit) directly, not `&mut State<DB>`. Updated `create_executor` (`Evm<DB,I>` / `DB: StateDB` / `Inspector<Context<DB>>`), both `ArcBlockExecutor` impl blocks (`E: Evm<DB: StateDB>`), and `builder_for_next_block`'s `BlockExecutorFor` DB arg → `&'a mut State<DB>`. Commits `87e643c` and earlier. (2 cosmetic warnings left: a test-only `State` import, deprecated `map_frame`→`map_item`.)
- **Next crate: `arc-execution-payload` (9 errors)** — reth 2.0 payload-builder API: `EthPayloadBuilderAttributes`/`PayloadBuilderAttributes` moved, `PayloadTypes::PayloadBuilderAttributes` assoc-type change, changed fn arities, and structs that gained fields (`execution_cache`, `trie_handle`, `payload_id`) needing pattern updates. After that: `execution-config/txpool/validation`, `node`, `malachite-app`. Same pattern, different API surfaces. The compile front is moving ~1 crate at a time; consensus-correctness validation (differential/e2e) is still the separate multi-week cost.
- **arc-evm: 10 → 8 errors.** Fixed 3 revm-36 API breaks (verified against revm source in `~/.cargo/registry/src/.../revm-*-{9,12,16,17}.0.0`): (a) `Bytecode` is now an opaque struct — use `eip7702_address()`/`new_eip7702()`; (b) `set_state_clear_flag` removed, EIP-161 clearing is unconditional in revm 36 (`revm-database` CacheState `// EIP-161 state clear`), so dropping the `(true)` call is behavior-preserving. Remaining 8 are STRUCTURAL reth-2.0-SDK (alloy-evm 0.30): `BlockExecutor::into_result` (must ADD per-tx `ResultAndState` accumulation to `ArcBlockExecutor` — it only tracks receipts/gas_used; ref impl `EthBlockExecutor::into_result` returns `self.result`), `create_executor` signature change, and `Evm`/`Inspector<Context<...>>`/`DatabaseCommit` generic re-wiring (evm.rs:1787/1827/367). These touch block-execution result/state → consensus-critical; need Arc's differential test suite to validate, NOT just compilation. Do not blind-edit.
- **Layer 1 + 2a DONE & COMMITTED** (`c80c70d`, `2799354`, plus reth-primitives-traits/DownloadDefaults fix): deps resolve, revm unified to single 36. Key fixes: `reth-primitives-traits` moved to crates.io (use `version = "0.1"` not git — independently versioned 0.1.x, NOT removed); reth 2.0's `DownloadDefaults` (in `reth-cli-commands`) gained required `snapshot_api_url: Cow<'static,str>` field.
- **`cargo check -p arc-evm` now compiles the entire reth-2.0 + revm-36 tree and gets down to 10 real API errors** (all in `crates/evm/src/evm.rs` + `executor.rs`) — the revm-34→36 EVM-code port. These are SECURITY-SENSITIVE (EIP-7702 delegation, state-clear, block execution) — port carefully with revm 36 docs, do NOT guess. The 10:
  - `executor.rs:360` `set_state_clear_flag` — removed/renamed on revm 36 `State` (find successor in revm-database 12).
  - `evm.rs:986,4047-4049` `Bytecode::Eip7702(..)` — revm 36 made `Bytecode` a struct (was enum); use the new accessor/constructor in revm-bytecode 9.
  - `executor.rs:80` `create_executor` incompatible trait signature (alloy-evm 0.30 `BlockExecutorFactory`).
  - `executor.rs:355` missing `into_result` — `BlockExecutor` trait gained a method to implement.
  - `evm.rs:1786,1788,1826,1827` `Inspector<revm::Context<...>>` / `DB: DatabaseCommit` bound failures + `BlockExecutor::Evm == ArcEvm` type mismatches — revm 36 Inspector/Evm generics changed.
- After arc-evm: expect similar in `crates/evm-node`, `crates/execution-*`, then layer 3 (reth SDK node-builder/payload/Storage-V2), then datadir migration.

HONEST STATUS: dependency + reconciliation layers are DONE (big milestone — from "won't resolve" to "10 specific core-EVM API errors"). Remaining = deep semantic port of Arc's EVM executor to revm 36 + reth-2.0 SDK; multi-day, human-reviewed (consensus-critical). No working build yet, so **no real "reth 2.0 on Arc delivery number" exists** — do not fabricate one. Closest real proxies: warm-cache 235 Mgas/s (Arc reth 1.11) and reth 2.0's published ~2 ms state-root / 1.7 Ggas/s on large blocks.

## Payment-lane / UTXO experiment (branch `utxo-state-experiment`)

Goal: test the paper's payment-sector thesis empirically — can a UTXO payment state (a) stay in RAM
as tx volume grows, (b) be committed cheaply, and (c) beat reth + simple native (account) transfers?
Standalone crate **`experiments/utxo-state/`** (own `[workspace]`, detached from the reth fork; deps
cached: alloy-primitives, alloy-trie, k256/ecdsa, curve25519-dalek-ng, rayon). Bins
(`cargo run --release --bin <name>`):

- `utxo-bench` — RAM UTXO set + 3 commitment schemes. **25M UTXOs = 3.4 GB RAM; ECMH commit flat
  ~259 ms/block across a 250× state increase; MPT/Merkle full roots are O(n).**
- `merkle_acc` — hash-only incremental Merkle accumulator (no DLog). **Flat ~325 ms/block to 25M,
  5.5 GB RAM, ~11 µs/update** (constant depth). Hash-only commitment is forced if you drop DLog (the
  additive multiset hash is broken by Wagner's k-sum; ECMH needs a group).
- `two_el` — two-EL/two-root prototype: a mock CL fans out to an MPT-rooted EVM-EL and an ECMH-rooted
  UTXO Payment-EL **in parallel** and merges both roots into one header. Shows **block time =
  max(T_evm, T_pay)**; lane ~free while T_pay < T_evm (crossover ~10k payments/block at T_evm≈140 ms).
- `utxo_tx` — real secp256k1-signed UTXO payments end-to-end. **~8.5k tx/s single-thread, verify =
  75% of cost (~82 µs k256 ECDSA); state+Merkle ~3× cheaper.**
- `compare` — UTXO vs account-native, single-thread, same secp256k1+Merkle. **~equal (account
  marginally faster: fewer state writes).**
- `parallel_cmp` — parallel throughput. ~104k tx/s both, ~9× scaling. **(Had a bug — see below.)**
- `contention` — the corrected contention test (real balance RMW).

### KEY FINDINGS (honest — all measured on i7-11700F 8C/16T, 62 GB, testnet stopped for parallel runs)
- **For SIMPLE payments, UTXO is NOT faster than reth-style native transfers.** Single-thread and
  parallel they are ~equal (~105k tx/s on 16 threads), both **signature-verification-bound** (~9 µs/tx
  parallel k256 ECDSA; libsecp256k1 ~2× faster). Verification is contention-free and parallelizes
  identically for both models.
- **Account contention is real but invisible for simple transfers.** `contention` models REAL balance
  read-modify-write: hot recipients give 97% conflict and a hot account touched 1309× (non-commutative).
  Throughput is unchanged because the contended work (a balance add) is ~0 µs vs 9 µs/tx verify.
  Contention only bites when the contended *work* is expensive (contracts).
- **BUG I made and fixed (do not repeat):** the first `parallel_cmp` "account" path wrote a CONSTANT
  leaf and batched changes like UTXO — it never modeled RMW or hot-account serialization, so its
  "account == UTXO" was an artifact. `contention` fixes it. Also fixed a parallel-Merkle early-`break`
  that skipped levels to the root when changes collapsed into one subtree.
- **Where UTXO actually wins (architecture, not per-tx speed):** (1) a small **RAM-resident** payment
  lane avoids reth's disk-bound 169 GB shared MPT (App C: 0.1–0.37 Ggas/s on real state vs ~105k tx/s
  in RAM); (2) it **avoids the Block-STM contention machinery** (deterministic parallel, zero aborts —
  robust under adversarial contention); (3) conflict-free parallelism for EXPENSIVE/contract execution.
  The win is **isolation + small RAM state**, not "UTXO > account".
- **Commitment choice:** Merkle is fine. ECMH (32 B, O(1)) needs DLog → rejected. Hash-only ⇒ incremental
  Merkle (additive multiset hash is k-sum-broken). Mature alternatives: Bitcoin Core **MuHash**
  (incremental multiset hash, group-based, what `coinstatsindex` ships) or **libbitcoinkernel** (C++/FFI,
  full Bitcoin tx/Script semantics; needs cmake — NOT installed here). No reth-style EL uses Utreexo;
  Utreexo lives in Bitcoin nodes (utreexod, Floresta/`rustreexo`).
- **How Bitcoin scales sig verify (no batching):** libsecp256k1 + parallel `CCheckQueue` + **signature
  cache** (verify-once at mempool, skip at block connect). reth caches recovered senders similarly.

- `locality` — locality-keyed vs hash-keyed state+commitment (same Merkle shape, only the node
  store differs: contiguous Vec vs HashMap by node index = MPT-style). **Dense beats sparse
  1.1×→1.4× and the gap WIDENS with size** (dense −12% vs sparse −30% as it spills cache, to 12M
  entries). Honest scope: 1.4× is RAM-only (cache effect) and understates it — a real MPT keys by
  `keccak(addr)` (fully random) and pays on DISK (the 27.8 ms cold). So the data-structure win is
  modest in RAM (~1.4×), large on disk.

### REFINED THESIS (after the account-vs-UTXO + locality work — supersedes "UTXO is better")
- The 169 GB / 27.8 ms problem is the **shared general-purpose hash-keyed MPT**, not payment data.
- **First-order fix = ISOLATION:** a dedicated payment lane whose state grows with USERS (not tx
  volume or contract activity) → small → fits in RAM → no disk-bound merklization. Works for an
  account lane too; it is NOT a UTXO-specific win.
- **Pragmatic implementation = reuse reth** (EVM account + Block-STM + mempool + sender cache):
  payment-only account state is compact (≤ UTXO, ~1 entry/user), throughput = UTXO (~105k tx/s,
  signature-bound), and reth's MPT on a SMALL RAM state costs only ~1.4× vs an ideal structure —
  acceptable. This is the low-risk, high-reuse design.
- **Secondary refinement = a locality-keyed, unified state+commitment structure** (dense Merkle /
  Jellyfish-MT / Verkle): ~1.4× in RAM, more on disk; worth it at scale, not required.
- **UTXO is an ALTERNATIVE lane**, justified only by deterministic conflict-free parallelism (no
  Block-STM machinery/aborts) + statelessness — NOT speed or state size (both ≈ or worse than a
  minimal account lane). Do not pitch UTXO as faster/smaller.
- Architecture unchanged: second minimal EL + one extra header root; CL fans out + combines;
  block time = max(T_evm, T_pay), payment lane ~free while T_pay < T_evm.

DO NOT fabricate numbers. k256 (pure Rust) is ~2× slower than libsecp256k1.

### PAPER WRITING STATE (paper repo: `2026.arc.payment.highway.claude/main.tex`, Overleaf-connected)
The paper is being rewritten to a HONEST, NON-UTXO-LEANING framing the user approved:
**(1) the bottleneck is the shared general-purpose disk-bound MPT; (2) first-order fix = ISOLATE
payments into a lean lane (grows with users, fits in RAM); (3) pragmatic implementation = REUSE
RETH (account + Block-STM, Approach 2); (4) UTXO is an HONEST ALTERNATIVE (deterministic
conflict-free parallelism + statelessness), NOT faster/smaller; (5) the RESEARCH FRONT = the
COMMITMENT for the lean EVM state (unify store+commitment, locality-key it, cheap as it scales
past RAM, cheap proofs).** Push to Overleaf after each compile-clean change.

Done so far (all pushed, compiles clean ~25 pp): App C (reth-2.3 measurements), App D (reality
check), **App E "A payment lane in RAM"** (the experiments/utxo-state results — measured, honest
ranges, variance noted, reproducible via the crate README); **§4 pivot** (recommend Approach 2 /
reuse-reth, UTXO as honest alternative, tied to App E); **Research Questions** now lead with
"the commitment for the lean EVM state" (\S sec:rq-commitment) as the central front.

VERIFICATION (user demanded no fake numbers / reproducible): re-ran all experiments; logical
results are deterministic and reproduce exactly (utxo-bench 25M = **3.42 GB**, merkle_acc 25M =
**5.52 GB** both reconfirmed to the decimal). Timings vary ±~15% run-to-run; parallel throughput
is load-sensitive (92k contended vs 105k idle) — paper now states ranges, not point values.

DONE (end-to-end consistency pass, all pushed, compiles clean 25 pp): read the whole paper;
removed every UTXO-lean spot (abstract, §2 RQ list, §3 forward-ref, §4.3 "State and commitment"
now account-first, §4 takeaway now RECOMMENDS Approach 2, open-questions list). Moved the
memory/storage **schematic into the body §3** (replaces the stage-bar as `fig:exec-anatomy`,
"why I/O is bad"); App C keeps the cold/warm/V2 **bar chart**. Consolidated **§4.4** into a
"three measurements, one conclusion" synthesis table (reth shallow/big + Block-STM + UTXO-vs-account
→ isolate + reuse-reth + commitment-is-the-lever); moved the detailed Block-STM per-thread table to
**App E item 7**. Verified numbers reproduce (3.42 / 5.52 GB exact; timings stated as ranges).
Paper now consistent end-to-end: isolation + reuse-reth thesis; UTXO an alternative; the commitment
for the lean EVM state is the central research front. Avoid re-introducing UTXO-leaning or unmeasured
numbers in future edits.

### Dual-EL payment-lane implementation (branch `dual-el-payment-lane`)
Goal: each node = 1 CL (Malachite) + 2 ELs (EVM-EL + lean payment-EL, both reth); every block carries
TWO roots (evmStateRoot + paymentRoot), both built by the proposer and re-executed by every validator;
a dual-lane spammer stress-tests both. This is the paper's full design — **multi-day, consensus-critical**
(NOT one-night). Full file-by-file plan in **`docs/dual-el-payment-lane.md`** (commit e32d77c). Status:
branch + blueprint done; **no consensus code written yet** (deliberately — the riskiest piece, step 3, is
an SSZ + proposal-streaming change to `ConsensusBlock` that breaks consensus if rushed). Step order
(low-risk first): (1) 2nd Engine additive in eth-engine/config; (2) CLI+quake 2nd EL endpoint + 2nd
genesis; (3) `ConsensusBlock.payment_payload` field [HIGH risk]; (4) proposer builds both; (5) validators
re-execute both; (6) dual-lane spammer. Each consensus change must pass differential replay (all
validators compute identical paymentRoot) before trusting it.

**STEP 2 DONE + VERIFIED (2026-06-28):** `experiments/dual-el/launch-payment-els.sh` launches a 2nd
reth EL ("payment lane") per validator on the running localdev4 testnet (real `arc_execution` image,
own datadir `reth-pay`, genesis, ports http 19545/19645/19745/19845, discovery off, on
`arc_testnet_host-access` — NOT the `internal:true` `arc_testnet_default`, or published ports don't
route). Verified: 4 payment ELs up, RPC chainId=1337 block=0 (idle), while EVM ELs advance (blk 213+).
Gotcha: localdev EL uses IPC auth (no `jwtsecret` file) → don't pass `--authrpc.jwtsecret`; let reth
auto-gen `<datadir>/jwt.hex`. **STEP 1 NOT done** (CL driving EL2): coupled to step 4 and blocked by
the consensus Docker image rebuild (Cargo.toml reth `file://` fork unreachable in Docker build) —
do steps 1+3+4 together next. The base testnet here was started with `-e 50000 --monitoring false`
(blockscout fails otherwise); validator1 sometimes doesn't boot (3/4 is enough for BFT).

**STEPS 3+4 DONE — consensus code for two-root blocks compiles + tested (2026-06-28, branch `dual-el-payment-lane`).**
- STEP 3 (commit ad95465): `ConsensusBlock.payment_payload: Option<ExecutionPayloadV3>`; `SszBlock`
  7→8 tuple (ethereum_ssz supports Tuple9, verified); proposal streaming length-frames the two lanes
  (`[u64 len(evm)][evm ssz][payment ssz?]`); store encode/decode carry it. Round-trip test +82 db tests pass.
- STEP 4 (commit 0ef9046): CL builds+validates+finalizes BOTH ELs. config/CLI gained `payment_*`
  endpoint flags + `payment_engine_config()`; node.rs connects an optional 2nd `Engine`; threaded
  `payment_engine: Option<&Engine>` through run→go→handlers (started_round/get_value/
  received_proposal_part/process_synced_value/decided). `build_block` builds the payment payload on
  EL2's own head (`get_block_by_number("latest")`) aligned to the EVM timestamp; `validate_consensus_block`
  re-executes it (newPayload) — block valid only if BOTH lanes validate; `decide` forkchoices EL2 to the
  decided payment block. Single-EL path unchanged (None default). lib+bin+tests compile; 261 lib tests pass.
- **v0 limit:** value-sync carries only the EVM payload (synced blocks have no payment lane); the live
  proposal-streaming path carries both. NOT yet differential-validated on a live testnet.
- **TO RUN (remaining):** (a) payment genesis with blockGasLimit=100M; (b) revert the 41 reth `file://`
  deps in Cargo.toml → upstream `tag=v2.3.0` so the CL Docker image rebuilds (backup
  /tmp/Cargo.toml.preforkpatch); rebuild CL+EL images; (c) quake passes `--payment-execution-endpoint`
  (+ws/jwt) to each CL pointing at its payment EL; (d) boot, drive both lanes (dual spammer), confirm
  all validators compute identical paymentRoot. The Docker rebuild is the long pole.

**🎯 value_id NOW COMMITS TO BOTH LANES + 24h LIVE SOAK PASS (2026-07-02, commit `4c86b4f`).**
Closes the equivocation hole where the certified consensus value bound only the EVM lane (a proposer
could stream two payment payloads under one EVM hash → fork the payment lane beneath a valid EVM
commit certificate). Now `value_id = keccak(evm_block_hash ‖ payment_block_hash)` when a payment lane
is present, else exactly the EVM hash (single-EL blocks byte-for-byte unchanged, no migration).
- **types/block.rs:** `commit_lanes(evm, Option<pay>)` = single source of truth; `ConsensusBlock::value_id()`
  / `payment_block_hash()` (block_hash() stays the real EVM hash for EL ops); the two `From` impls
  (`ProposedValue`/`LocallyProposedValue`) vote on `value_id()`; `DecidedBlock` carries `payment_payload`,
  `new()` re-checks the commitment vs the certificate, `from_stored_evm_only()` for DB reconstruction;
  extracted `frame_lanes`/`unframe_lanes` (shared by streaming + sync).
- **consensus-db/store.rs:** undecided blocks keyed by `value_id` (not EVM hash) so the decide-path
  lookup by `certificate.value_id` resolves; DB reconstruction uses `from_stored_evm_only`.
- **sync now carries BOTH lanes (closes the v0 gap above):** `process_synced_value` unframes both,
  re-validates the payment lane, dedups by value_id; `get_decided_values`+`app.rs` thread
  `payment_engine`, fetch EL2 payloads per height, verify the fetched lanes reproduce the certificate
  value_id before shipping, frame both lanes. Decided store still persists EVM-only (no schema
  migration); the authoritative both-lane commitment lives in `certificate.value_id`, payment block
  is canonical in EL2.
- **Tests:** 6 new `block::tests` incl. the equivocation guard (`value_id_changes_when_only_payment_lane_changes`),
  order-sensitivity, frame/unframe round-trips. types 167 + consensus-db 82 + consensus lib 261 pass.
- **24h SOAK PASS:** rebuilt the CL image from this commit, ran `soak4` (4× CL+EVM-EL+payment-EL) with
  continuous dual-lane spam (300 tx/s/lane). **1422/1422 60s-checks, 0 failures over 86,400s**; both
  lanes lockstep to ~329k blocks; per-lane block hash identical across all 4 validators at every settled
  height (→ all validators computed an identical value_id every height); 12/12 containers, disk fine.
  A nondeterministic/mismatched commitment would have halted or forked within minutes — it did not.
  Harness: `experiments/dual-el/soak.sh` (this run used a path-adapted copy for the `arc-node-paymentlane`
  clone). NOTE: two clones of this branch exist — `/home/papaduck/arc-node` (older, prior soak) and
  `/home/papaduck/arc-node-paymentlane` (this commit); they have diverged.

**DUAL-EL DEMO TOOLING + STATE-BLOAT/PRESEED RUNS (2026-07-10..13, commits e91ff14..8a36e23+).**
Tooling (experiments/dual-el/): `demo.sh` (start/stop/status: quake soak4 + payment ELs + dual load +
dashboard; stop DELETES datadirs), `demo-bloat.sh` (state-experiment variant: preseeds 10M payment
accounts in genesis at start — regenerated EVERY run — guzzler bloat on EVM lane, pool-update load on
payment lane, memory caps applied at start), `demo-empty.sh` (clean chain, no preseed/load),
`dashboard.py` (localhost:8080: per-lane block/root/tx cards, value_id callout via cast keccak,
state-vs-history split [state=db/ = what a pruned snapshot ships; history=static_files+rocksdb minus
RocksDB WAL churn], touched-accounts counter, side-by-side headers, per-lane exec_ms+root_ms from reth
prometheus + disk I/O + RAM from cgroups, block-production rate; settled height anchors on MAX head so
an offline validator can't pin the display; "agree"=no-fork at settled height, NOT all-in-sync).
Spammer additions: `--fresh-recipients` (every transfer to a new address = new account/tx; wall-time-
seeded counter), `--recipient-pool base:size` (seq walk w/ wrap: first pass creates pool, then pure
balance updates — separates onboarding from steady-state payments). Payment ELs gossip via static
admin_addPeer mesh (launch-payment-els.sh; un-fed validator otherwise proposes empty payment blocks on
its round-robin turn). PAYMENT_GENESIS env selects payment-lane genesis.

KEY FINDINGS (all measured; trend log ~/dualel-bloat-trend.log):
1. **Genesis-resident alloc RAM**: reth keeps the entire genesis alloc in process RSS forever
   (~250B/account): 10M preseed = ~2.7G/EL, 30M = ~8G/EL, on top of disk state (10M=0.66GB, 30M=2.1GB
   MDBX) and working caches. 100M preseed = 7.2GB genesis JSON, ~30-60min parse+root per EL BOOT (OOM
   cascade if boots overlap; must boot sequentially, uncapped, then clamp). Production: don't preseed
   huge allocs; organic/tx-created accounts live only in DB.
2. **Proposer build-failure crash-exit (FIXED 509f404)**: payment getPayload timeout during get_value
   crashed the CL → restart-loop on same height → quorum loss + stall. Fix: log + skip round (same as
   existing None path). Confined to our get_value.rs handler, no consensus semantics.
3. **Value-sync batch livelock (OPEN)**: both-lane sync batches (10 heights × ~500tx payloads ≈ 2.2MiB)
   exceed client request timeout under load → lagging validator loops request/timeout forever and its
   request storm taxes healthy peers. Fix pending: value_sync.batch_size 10→~3 in scenario config.
   (Sync itself validated live: val2 caught up 2000 heights with both lanes re-validated — 4c86b4f.)
4. **Shared-box ceiling + caps**: 12 nodes on 62G thrash-froze uncapped under heavy state loads (swap
   sawtooth; sheds every ~45min; one hard machine freeze, no oom-kill logged — hardware suspected).
   Per-container caps (docker update, live) fix isolation: one container OOMs alone (137, ~3min
   recovery) instead of box death. Budget caps for END-state (EVM ELs pinned at 97% of 2.5G once the
   guzzler trie grew → 0.16blk/s; 5G fixed it). Current: pay 10G / evm 5G (bump demo-bloat from 2.5G!)
   / cl 0.75G. After ANY EL pool wipe (restart) spammers MUST be bounced (-l re-syncs nonces) or all
   their txs queue forever nonce-gapped.
5. **Latency coupling measured**: one certificate/height couples cadence = max(T_evm,T_pay)+overhead.
   Guzzler-full EVM blocks (50-90M gas keccak+cold-SSTORE, all txs serial on ONE contract+counter — the
   hot-account worst case; executed TWICE: build+re-execute) drag the pair to 0.1-0.7 blk/s while
   payment exec stays ~5ms. Light lanes hit ~3.8-3.9 blk/s (24h soak) ≈ the 250ms target. Builder
   deadline bounds block FULLNESS, not cadence (2000→250ms changed nothing under saturated mempool).
6. **RUN7 3h averages (10M preseed, cumulative histograms)**: EVM(guzzler) root 10.79ms/exec 57.25ms
   per block (~18tx) vs PAY root 2.59ms/exec 5.20ms per block (273-4761tx) → per-tx gap 2-3 orders of
   magnitude; payment transfers ≈ 33µs/tx exec on 10M accounts; payment proposer builds full blocks
   ~40x faster (exec proxy). Zero forks/divergence across ALL runs incl. OOM kills, CL restarts,
   460-height sync catch-up. Payment state root flat 1-11ms on 10M throughout.
7. Ops runbook: CL "Manual intervention required" park after EL races → docker restart CL once ELs up;
   exit-137 pay EL → boot uncapped (60g) then clamp; /tmp wiped on reboot → trend log in home dir.
RECOMMENDED DEMO CONFIG: 10M preseed + fixed CL (509f404) + caps pay10/evm5/cl0.75 + batch_size fix
before 4-validator demos. demo-empty.sh for clean-slate demos. Exec benchmark (empty vs 10M preseed,
identical 1500tx/s transfers both lanes, 1h each): results land in /tmp/execcmp2-results.txt (2026-07-13).

**MULTI-MACHINE FLEET (2026-07-14..16, branch `fleet-multi-machine`).** 4 validators on 4 tailscale
machines, driven entirely from ginny-alienware: val1 here (62G, wifi) + val2 ginnythui
(100.85.150.119, 62G, wired) + val3 papaduck (100.70.62.92, 78G, wired) + val4 papaduck-alien2
(100.86.97.40, 15G, wifi). tailscale ssh enabled on remotes; arc images shipped via docker save|gzip|ssh
docker load. LAN 2-8ms.

Topology mechanics (gen-fleet.py rewrites quake's compose): CL P2P = persistent-peers multiaddrs
/ip4/<host-ts-ip>/tcp/2700(N-1) (host-published); EVM EL P2P = trusted-peers enodes @<ts-ip>:3030N +
published 3030N:30303; payment ELs publish 3041N:30303 + admin_addPeer ts-enodes. Remote composes:
per-host compose-valN.yaml, volumes under /home/papaduck/arc-fleet (val3 fast variants:
/mnt/blockchain.ssd/arc-fleet), networks recreated per host with DYNAMIC subnets and NO static
container IPs (remote docker pools collide, e.g. ginnythui wiki owns 172.21/16); pay ELs MUST run
host-access as PRIMARY network + connect internal after (internal-primary silently breaks port
publishing AND the CL never reaches them). CL<->EL stays machine-local (IPC).

Machine gotchas (all cost real time): papaduck docker couldn't publish ports (iptables DNAT missing
kernel module after kernel upgrade) -> REBOOT fixes; alien2 wifi truncates GB-scale ssh/tar pipes ->
chunked base64 (8MB pieces, per-chunk sha+retry) or taildrop (needs operator); docker auto-creates
root-owned mount dirs -> every later user-extract silently fails (chown -R in clean step now);
alien2 wifi power-save = 101ms idle RTT -> keepalive ping (nohup ping -i 0.3 gw) -> 7ms; remote
cleanup MUST use explicit container names (three different silent failures with $(docker ps) subshells
inside tailscale ssh strings).

CL changes: value-sync env overrides ARC_VALUE_SYNC_BATCH_SIZE / ARC_VALUE_SYNC_TIMEOUT_SECS
(hardcoded 10 blocks x 1s timeout livelocked lagging validators; fleet runs 3/15s via gen-fleet env
injection; validated: 1151-block wifi catch-up in 5min). ARC_PAYMENT_GENESIS_FILE_PATH (per-lane
V4/V5 fork detection) lives on the gravity branch build. FINDING #7 (OPEN): pay-EL OOM that loses
unpersisted blocks while CL tip is ahead -> value-sync can't backfill (<tip) and ProcessSyncedValue
treats newPayload=Syncing as FATAL -> CL crash-loop. OPS HEAL (scripted in revive-val.sh): manual
engine_forkchoiceUpdatedV3(canonical head) -> reth p2p backfills from gossip peers (2148 blks/2.5min)
-> restart CL. CODE FIX PENDING: treat Syncing as retryable in process_synced_value.

Builder deadline: quake scenario had 2000ms per lane; CL gives proposer 3s for BOTH lanes -> 2+2>3
guarantees timeout on a contended host (papaduck-as-desktop: every-4th-height 4s stall, measured
gaps [4,0,1,0...]; round-robin means a slow proposer taxes EVERYONE ~3.5x - tolerated != unaffected).
Now 500ms everywhere (soak4.toml el.config + all launchers); deadline bounds packing, NOT cadence.

DISK PERSISTENCE IS FIRST-CLASS: reth_consensus_engine_persistence_save_blocks_duration histogram;
persistence is async but caps SUSTAINABLE cadence <= 1/persist-per-block (buffer bounded, then
backpressure). fsync(4k dsync x100) per machine: alien2 0.67ms, ginny 1.38, ginnythui 1.44,
papaduck-home 30.9ms(!) -> papaduck /mnt/blockchain.ssd 1.5ms. v3 persist 3704ms/blk on home disk ->
67ms on NVMe (55x). Full 4761-tx pay blocks persist ~212ms/blk. Dashboard shows persist per validator.

Spam delivery: each ws send awaits ack -> remote/wifi targets gate the loop: 4-target 1.5k tx/s vs
LOCAL-only 5-8k+ (gossip fans out; spam-fleet.sh defaults local now; spam-fleet-fanout.sh keeps the
old behavior). Full blocks need delivered >= 4761 x cadence; a HEALTHY chain (3-4 blk/s) needs 15-19k
tx/s to fill — full blocks are a symptom of slowness (or the coupling demo via guzzler).

MEASURE-2H (2026-07-15 20:35-22:35, all-fixes fleet, 10M preseed, pay blocks FULL 4761tx the whole
window, 6009 blocks): per-blk PAY exec 64.4 / root 12.0 / persist 211.8 ms; EVM(~5tx guzzler) exec
16.0 / root 13.4 / persist 121 ms. Per-tx: PAY 13.5us exec / 2.5us root / 44us persist vs EVM ~3.2ms /
~2.7ms / ~24ms (240x / ~1000x / ~540x). val4 partial (mid-window OOM+revive). Results:
/tmp/fleet2h-results.txt; harness fleet/measure-2h.sh.

Fleet scripts (experiments/dual-el/fleet/): demo-fleet.sh (4-machine 10M ~40min), demo-fleet-fast.sh
(10M + val3 on /mnt/blockchain.ssd), demo-fleet-empty.sh (~5min clean), demo-fleet-empty-fastssd.sh,
demo-fleet2.sh (2-machine), spam-fleet.sh / spam-fleet-fanout.sh (start|stop|status; env rates),
revive-val.sh [N] (single-validator revival incl. finding-#7 heal), measure-2h.sh, gen-fleet.py /
gen-remote-val4.py (compose surgery), pilot.sh. All starts self-verify + print fallback; loads always
separate. **`clean-fleet.sh`** (also a `clean` subcommand on demo-fleet-metamask/-fast/-empty-fastssd)
= deep-wipe ALL machines: force-rm every `validator*` container + every datadir (local `.quake/*` except
monitoring, each remote's `arc-fleet` on home + NVMe) + buildx prune. Use it — start/stop only touch a
tracked scenario, so ORPHANED datadirs from past runs pile up (measured: ~615 GB across the 4 machines
in one session; the docker "build cache" number in `system df` is a PHANTOM over-count — the real hog is
always the `.quake`/`arc-fleet` chain datadirs, 50-200 GB/machine/run). `clean` is explicit-only (never on
start/stop); datadirs are root-owned so removal needs a root container (`docker run --user root alpine rm`). Dashboard fleet mode: DUALEL_FLEET=<json {valN: ts-ip}> (unset = single-machine); rows:
per-val root/exec/PERSIST now+avg-run, landed TPS, block rate; exec-now had been lifetime-avg since
inception (fixed), stale values expire after 45s idle.

**PRODUCT-LEGIBLE DEMO: "state grows with activity, not payments" + "EVM congests, payment lane doesn't"
(2026-07-18..20, branch `fleet-multi-machine`, commits `3390c40` + `525cff1`).** For product-audience
demos (MetaMask walkthrough on single-machine or fleet, EVM lane chainId 1337 / payment lane 1338).
Two theses, each measured live (honest, real EIP-1559 — not mocked):
- **State metric = `reth_db_table_size{table=...}` prometheus gauge** (live, no DB lock), summed over
  STATE_TABLES {HashedAccounts, HashedStorages, AccountsTrie, StoragesTrie, PlainAccountState,
  PlainStorageState, Bytecodes} = exactly what a pruned snapshot ships. `du` on the datadir is USELESS
  (MDBX pre-allocates a flat ~4GB file). Payment state grows in HashedAccounts (users); EVM state grows
  in HashedStorages (contract activity). Measured live: pay ~10MB (transfers between existing accts add
  ~0) vs EVM ~58MB and climbing (storage writes + fresh recipients).
- **Congestion**: oversubscribe the 30M EVM block (~200 tx/s of ~1.6M-gas guzzlers = ~320M gas/s demand
  vs ~120M/s capacity) → block pins ~95% full, EIP-1559 baseFee ratchets +12.5%/full block (compounds
  ~2× every ~6 blocks; measured 12k→29M wei in 2min, exponential, no plateau in that range; seen to reach
  ~1e12 wei / 1000 gwei at steady state), mempool holds ~740 stuck txs. Meanwhile the 200M payment lane
  at ~300 tx/s transfers (~6M gas/s) stays <1% full, baseFee flat at the **49-wei floor**, backlog drains
  each block. NOT a protocol-fixed fee — it's capacity≫demand keeping the fee at the floor (state honest).
  **SUPERSEDED (2026-08-04): the payment fee IS now protocol-fixed.** Branch `payment-lane-gas` (other
  session) built `fleet/set-lane-economics.sh`: `updateFeeParams` with **minBaseFee==maxBaseFee**
  (default 1 gwei) pins the payment lane's baseFee EXACTLY (verified: 1,000,000,000 wei constant), while
  the EVM lane stays dynamic. Ported to `fleet-multi-machine`; demo-metamask starts auto-apply it
  (steps 4c/8c, 5× retry — fresh-chain quorum races silently drop controller txs). Cost per transfer =
  21000×1e9/1e18 = **$0.000021 flat** (native gas token is USDC, 1e18 wei = $1); dashboard shows $ per
  transfer on both lanes. The EVM lane gets **Arc MAINNET's real fee band** (min 20 gwei / max 20,000
  gwei from assets/mainnet/genesis.json) — an idle devnet otherwise decays to the 1-wei dev floor and
  reads CHEAPER than the payment lane, inverting the story. Payment fixed fee = 20 gwei too
  (matches the EVM floor): BOTH lanes idle at the SAME $0.00042/transfer — apples-to-apples — and under
  congestion only the EVM lane climbs (→$0.42, 1,000×); spammer-safe (signs max_fee 40k gwei). NOTE: `payment-lane-gas` also reorganized the demo surface (testnet.sh/spam.sh,
  attic/) — the two branches have overlapping parallel work (EXTRA_ACCOUNTS, --account-offset); reconcile
  before merging either.
- **Tooling (experiments/dual-el/):** `congest-demo.sh` {start|watch|status|stop} (auto-detects each
  lane's chainId from RPC; env EVM_RATE/PAY_RATE/GUZZLER/EVM_WS/PAY_WS; `watch` = live fullness-bar/
  baseFee/backlog table). `congestion.py` + `state-growth.py` = reproducible generators → CSV + self-
  contained presentation SVGs. `dashboard.py` `/product` = 3 hero tiles + congestion strip (fullness bar,
  fee-vs-floor multiple, mempool backlog via txpool_status) + state-grows chart; engineering view stays
  at `/`. `demo-metamask.sh` / `fleet/demo-fleet-metamask.sh` = MetaMask-tuned starts.
- **BLOCK CADENCE = 2 blocks/s (2026-08-04, commit 05317bd, product feedback: match Arc's real block
  time).** The CL paces heights via malachite "stable block times": each height it eth_calls
  `consensusParams()` on ProtocolConfig (0x3600...0001, PRIMARY/EVM lane only) and waits
  `targetBlockTimeMs` (0=unpaced; storage slot 0x668f...38520**5**, bits 112-127 of the packed
  8×uint16 ConsensusParams; localdev genesis ships **250ms**). **`set-block-time.sh <ms>`** flips it at
  RUNTIME via `updateConsensusParams` — onlyController = **hardhat dev account #8**
  (0x23618e81..., key at m/44'/60'/0'/0/8 of the junk mnemonic); effect within ~2 heights, no
  restart. demo-metamask.sh + fleet/demo-fleet-metamask.sh apply `BLOCK_TIME_MS` (default **500**)
  post-boot. Validated: both lanes lock to exactly 2.00 blk/s; dashboard Settlement reads ~0.5s.
- **SINGLE-MACHINE PORTABILITY (2026-08-06, commits 3d2c9ae + 4e01573; USER-CONFIRMED WORKING
  2026-08-07):** the whole demo (4 validators = 12 containers + dashboard) runs on ONE machine via
  `demo-metamask.sh`; validated cold-start end-to-end. `check` treats ports held by our OWN running
  demo as info ("stop first"), only fails when something ELSE holds them.
  `./demo-metamask.sh check` = preflight for a NEW device (binaries, images, node>=20-even,
  node_modules, ports, RAM). To port: clone repo + `cargo build --release -p quake -p spammer` (or copy
  binaries) + ship images (`docker save arc_execution arc_consensus | gzip` → `docker load`) + foundry +
  nvm node 22 + `npm install`. Footprint measured: **1.7 GiB idle / 5.5 GiB under full congestion** —
  16 GB device comfortable (lower the caps in start() below that). Dashboard + RPCs bind 0.0.0.0, so
  other devices reach them via the host's IP. Bench button caveat: run-bench.sh drives the FLEET
  distributed spam (tailscale) — on a lone device it degrades to val1-local spam only.
- **GOTCHAS:** (1) MetaMask reserves chainId 1337 for its built-in "Localhost 8545" — but the demo KEEPS
  1337(EVM)/1338(payment); do NOT change chainIds (breaks the working MetaMask demo — firm user
  constraint). The CL only validates the PRIMARY (EVM) engine's chainId against its whitelist {MAINNET
  5042, TESTNET 5042002, DEVNET 5042001, LOCALDEV 1337}; the payment EL's chainId is NOT validated, so
  1338 is fine there. (2) Guzzler intensity: `storage-write=100@8`≈182k gas/tx, `@80`≈1.6M gas/tx (use
  @80 — ~19 fill a 30M block AND land); `@600`/`@40` at high rate → per-tx gasLimit estimate exceeds the
  30M block → "-32003: gas limit too high", ALL accounts skipped, spammer EXITS. `-a N` must be divisible
  by `-g` (generators). (3) baseFee is PERSISTENT chain state — for the live fee-CLIMB visual, `stop` and
  let the EVM lane idle ~1-2min first (empty blocks decay it 12.5%/block back to the floor) THEN `start`;
  otherwise it opens already-maxed (still shows the divergence, just no ramp). (4) Payment-lane genesis:
  chainId + 200M gas set in BOTH the header gasLimit AND the ProtocolConfig slot (Arc reads gas limit
  from contract 0x3600...0001 slot 0x668f09ce... at runtime, not just the header).

**🎯 1 Ggas PAYMENT-LANE THROUGHPUT EXPERIMENT (2026-07-21, branch `fleet-multi-machine`).** Question:
how high can payment-lane tps go, and is "bigger blocks" the lever? Answer: **1 Ggas blocks WORK**
(chainId 1338 bounds are [1M,1B] in `crates/execution-config/src/chainspec.rs:293-297`, NO code change
— set via payment genesis header gasLimit + ProtocolConfig slot). **Block size is NOT the bottleneck.**
- **Tooling:** `PAY_GAS`/`EXTRA_ACCOUNTS` now env-overridable in `demo-metamask.sh` + `fleet/demo-fleet-metamask.sh`
  (defaults unchanged). Delivery: **`fleet/spam-fleet-distributed.sh {start|stop|status}`** — the TRUE
  distributed spammer (didn't exist before; fleet spam was always local-from-ginny + gossip): runs S
  spammers ON EACH machine against that machine's LOCAL payment EL (ws 19546/19646/19746/19846),
  globally-disjoint account ranges (`--account-offset`), `-l` nonce-resync for re-runs. `start` folds in
  an idempotent `ship` (sha-check, base64 over `tailscale ssh`, binary is only 5.1MB). Measure:
  **`pay-throughput-bench.sh {run|measure|stop}`** (`measure` = sample-only; reports avg + PEAK tx/s via
  best rolling ≥10s window, block rate, txs/block, fullness, exec/root/persist from prometheus).
- **Runbook:** `PAY_GAS=1000000000 EXTRA_ACCOUNTS=16000 fleet/demo-fleet-metamask.sh start` → `S=4 ACCTS=1000 fleet/spam-fleet-distributed.sh start` → `WINDOW=300 pay-throughput-bench.sh measure` → `spam-fleet-distributed.sh stop`. (S=4,ACCTS=1000 needs 16000 prefunded accounts = 4·S·ACCTS.)
- **RESULTS** (i7-11700F 16-thread local val1 + 3 tailscale remotes, **alien2 on WIFI**):
  - single-machine (all 12 containers + spam on ONE box): **7.4k tps, blocks 19% full** — CPU-contention +
    ack-gated delivery bound; MORE spammers made it WORSE (16 spammers = 6.5k). NOT block-size bound.
  - fleet + distributed spam: blocks **FILL to 100% = 47,618 txs** (1e9/21000). **5-min window: avg 9.5k /
    PEAK 15k tps** (snapshots to ~19k), block rate 0.28/s, 34,363 txs/blk (72% full), **exec 379ms,
    state-root 0.8ms, persist 169ms.**
- **KEY FINDINGS (this is the slide material):**
  1. **STATE-ROOT IS FREE ON A LEAN LANE: 0.8ms for a 34k-tx block.** Because 16k accounts send AMONG
     THEMSELVES → tiny ~16k-leaf RAM-resident trie, and NO new accounts → trie structure static, only
     values change. Contrast 27.8ms for a 2.7M-gas block on the 169GB disk-bound mainnet trie. The MPT
     "bottleneck" is about SHARED DISK-BOUND state, NOT a lean payment lane. **THESIS VALIDATED.**
  2. **EXECUTION IS THE SOLE BOTTLENECK:** exec 379ms/pass, **replayed ~5×/height** (proposer build + 4
     validators re-execute). Block time 3.6s (0.28/s) but measured work only ~0.55s → **~3s is CONSENSUS
     COORDINATION** (re-exec passes + voting + the **alien2 WIFI node** dragging rounds).
  3. **CONTENTION is the parallel-execution research problem:** transfers among a hot closed account set
     create read-write deps on balances → naive optimistic parallel (Block-STM) aborts. Deterministic/
     partitioned parallel (or UTXO conflict-free) is the angle.
  4. **STATE-ROOT GROWS WITH NEW ACCOUNTS:** cheap root holds ONLY for a closed set; fresh-recipients grow
     the trie → root climbs toward mainnet. "State grows with USERS, not payment volume."
- **PATH TO 20-50k:** parallel execution (cut the 379ms, replayed 5×) + drop the wifi node / reduce
  coordination. If cadence were exec-bound only (~0.93s/blk) the same full blocks = **~37k tps**. Block
  size and commitment are NOT the levers. TODO: isolate wifi-node drag (run 3 validators); try parallel exec.
- **GOTCHA:** after a spam run's timer expires, accounts are left nonce-gapped → mempool shows big
  `queued` (NOT `pending`) that never executes (waiting for nonces that won't come); drains/evicts on its
  own. Re-runs MUST use `-l` (in the script) or start at nonce 0 → "nonce too low". The product dashboard
  summed pending+queued, so it showed a phantom backlog — payment-lane pending tile removed for this reason.

**🧱 BUILDER SEPARATION PHASE 1 CLOSED (2026-08-17, branch `builder-separation`): v1.1 is the
result, v1.2 measured NEGATIVE and was reverted.** Dedicated builder EL (papaduck NVMe) + CL-side
prebuild stash. v0 (on-demand remote build) = -5..-10% (build stayed on critical path). v1.1
(decide-time kick, dual timestamp t0/t0+1, commit b888ce6) = **+7% @150M / parity @300M / +11%
@500M (18,649 tps)**, hit 54-82%. v1.2 (feed+kick at VALIDATION time, 3 timestamps) = **-5/-8/-9%
across 150/300/500M, REVERTED (ea6e92c)** — three structural causes from live logs: builder must
EXECUTE the fed block before builds start (get_value wins the race → candidates=0 dominant miss),
stretched rounds blow even a 3-ts window (ts_needed = ts_prebuilt+4..6s observed), and 8
full-payload feeds/height (4 validators × validation+decide) contend the builder + burn CL-side
serialization on the critical path (val3, co-located with builder, worst hit rate 31%). LESSON:
kick timing is NOT the lever; further prebuild gains need pending-visibility work — parked.
Full record: docs/deferred-exec-100k.md RESULTS. NEXT RUNG: vote-on-hash (consensus-on-hash +
total STF + lagged state root, docs/deferred-exec-100k.md) — user-authorized CL work, this branch.

**🎯 VOTE-ON-HASH INCREMENT 1 MEASURED POSITIVE (2026-08-18, branch `builder-separation`):
+16%/+8%/+27% tps @ 150/300/500M — 23.5k tps @ 500M breaks the old ~20k asymptote.**
`ARC_PAYMENT_DEFERRED_EXEC=1` (default OFF): payment-lane vote gates on STRUCTURAL validity only
(rebuild header via into_block_raw + Arc conventions pbbr=parent_hash AND Prague
requestsHash=sha256(empty) — BOTH must be set explicitly or every real block is rejected, unit
tests were circular, live gate caught it); EL2 exec runs in the vote gap (fire-and-forget spawn at
validation + proposer post-build) and is anchored at decide BEFORE commit (idempotent; INVALID =
loud deterministic halt before persist — measured live: all-Nil votes, stall, zero divergence).
Sync path still fully re-executes (divergence alarm). Decide-anchor wait 1.3-1.8ms/height (exec
always fits the gap). Paired fleet drains, deferred arm on OLDER chain: control 486/912/1,284ms
vs deferred 420/845/1,013ms. Per-tx saving 5-11µs ≈ half the ~20µs slowest-peer re-exec term.
Gates: 265 lib tests, 120-height dual-lane agreement x4, CL restart leg, structural-reject halt
demo'd. Commits 7d081ce..2a2e06d + requests-hash fix. CAVEATS: n=1/size/arm; total STF still NOT
in the EL (byzantine proposer = synchronized attributable halt, not fork); experiment-only.
Record: docs/deferred-exec-100k.md. Next rungs: compact blocks (~18µs/tx transport), lagged root.

**🚚 TRANSPORT CAMPAIGN (2026-08-23, "match reth first" — user-directed): gossip redundancy
KILLED (shim v1.2), residual = CL value-sync feedback loop.** Height decomposition of drain
heights (val2, 1.65MB N=100 blocks, 1.48s median): **stream+assemble 843ms (57%, p90 4.8s!)**
· votes 195ms · anchor (ecrecover+exec+fsync) 145ms · validate 23ms — signatures are
irrelevant (575/block); TRANSPORT is the wall. NIC forensics (alien2/wifi): v1.1 moved
**11.2 MB per 1.65MB block** (RX 5.24 + TX 5.93/blk) — push-on-append gossip = every node
POSTs the full block (base64 2.2MB) to 3 peers, 100%% redundant on a healthy chain (every CL
anchors its own node). **Fix: shim v1.2 (fork `6f3b9e4`) announce/pull** — ~100B announcement
{commitment,number}; unknown -> pull/backfill from announcer; bandwidth gate: in-sync peers
exchange ZERO block bytes; 3-node kill/restart still self-heals (validated live: val4 healed
via announce->backfill on deploy). RESULT: drain cadence 1.00->1.32 blk/s, RX 5.24->3.43.
**RESIDUAL (open): CL value-sync feedback** — under load val1+val2 fall behind ~every height
(122+170 syncs/4min) and get served FULL 1.65MB both-lane frames (val4's CL served 160/3min =
1.5MB/s = the remaining 4.45MB/blk TX). Loop: stream tail -> missed round -> value-sync ->
extra bytes -> worse tail. reth-era fleet moved 2.9MB blocks at 1.0s heights on the same
machines — matching it needs the sync-storm broken (why do healthy validators miss rounds at
0.7s heights: stream vs round-timeout race) and/or compact proposals. Lean drain best so far
~29-45k outputs/s (window-variance ±2x from wifi); replay bench: lean EL alone = 2.1-2.5
Ggas/s / 404k outputs/s / 2.48us/output (lean-replay-bench.py, 6,589 blocks byte-identical).
Tx wire: 72+28N B (N=10: 352B/35.2B per payment; floor 28B/output).

**✅ 2 BLK/S UNDER LOAD: HOLDS — 50M lean budget = 1.97 blk/s, 354/354 budget-full blocks,
14,080 outputs/s sustained (2026-08-22 night).** The frontier, all points budget-full/chain-
limited: 50M -> 1.97/s / 14.1k outs/s / ~0.5s · 300M N=10 -> 0.62/s / 26.0k / 1.6s · 300M
N=50 -> 0.44/s / 24.6k / 2.3s. Same law as the reth lane (height = floor + per-output x n);
budget is the latency/throughput dial. vs reth lane at the same 2 blk/s + 50M: ~4.6k tps ->
14.1k payments/s at 1.4k sigs/s admission (~10x less admission work per payment). Operating
recommendation: 50M for the 2 blk/s product point, 300M for the ~26k throughput point.

**🏁 FLEET N-SWEEP MEASURED — I5 DONE (2026-08-22 night): outputs/s PLATEAUS ~25k, per-OUTPUT
bound (~40us/output), fan-out advantage fully banked by N=10.** 4-machine fleet, fixed stack,
governed batch feeders (ws spammers = admission-vs-build contention, only 240 txs/blk; the
batch feeder = budget-PERFECT blocks): **N=10: 26,046 outputs/s, 111/111 blocks at 42,250/42,253
budget, 0.62 blk/s, 2,605 tx/s admission · N=50: 24,593 outputs/s, 80/80 at exactly 55,350,
0.44 blk/s, 492 tx/s.** Height cost 38-41us/output at BOTH N -> per-output, not per-signature;
bigger budgets won't help (blocks grow, cadence drops pro-rata). Lean node standalone executes
at 1.6us/output -> ~90%% of the 40us is CL-side (proposal streaming ~28B/output, per-node anchor
exec, voting). 100k outputs/s levers: compact lean proposals (hash/delta streaming), deferred
anchor exec, cut re-exec passes. TWO MORE BUGS closed en route: (7) proposer never stashed its
own build (self-delivery raced 40ms decides -> every own-proposal anchor via 5s grace; fix
718bd59 build-time stash) + started_round replay used store-loaded blocks (lying value_id,
lean_shim=None, no stash -> early-streamed proposals nil'd 25-48%% of turns; fix in same image:
replay ASSEMBLED blocks, stash, thread shim, skip store-only in lean mode). (8) LEAN NODE POOL
never re-based per-sender nonces on append (no canonical-state-update path in a standalone lane)
-> ANY multi-nonce stream demoted queued-forever after block 1 (latent since day 1, invisible to
1-tx/sender gates; fork fix 6f9521f pool.update_accounts on commit + streamed-nonce gate).
FLEET IDLE = 1.95 blk/s, 0 slow heights/937, p99 1.03s (= single box). OPS BANKED: remote
background launches need `(setsid nohup ... &)` + </dev/null (tailscale-ssh teardown kills
plain `&`); pkill -f in an ssh string matches the SHELL carrying the launch text (split kill
and launch into separate ssh calls); zsh does NOT word-split `set -- $var`; papaduck MagicDNS
broken -> use raw 100.x IPs in --peers; wipe lean datadirs between sweep legs or recovery
resurrects old nonces and the fund-file step silently skips.

**🎯 CADENCE SOLVED — 1.94 blk/s AT THE PACER TARGET (2026-08-22 morning, shim v1.1 stack):
0.58 -> 1.94 blk/s, 7s-mode GONE (1 slow height in 1,162), anchor failures 24-174/10min -> 0/0/0/0.**
The morning's 0.58 was a CHAIN of glue bugs, not lean fundamentals: (5) stash clear() wiped
next-height entries validated during decide(H) -> every anchor entered the peer-fetch race,
whose loser was late for its next proposer turn -> laggard role MIGRATED between validators
(fix d5909c3: remove only decided key). (6) fail-fast anchor turned lag into restart-height
churn (fix 09e06f9: Engine-API SYNCING semantics — new_block returns Valid|Syncing,
wait-and-poll 30s). (7) tip-linkage validation rejected sync replays of HISTORIC lean blocks
once the self-healed node ran ahead of a lagging validator's consensus -> value-sync wedged
(fix 7f18196: already-canonical branch — valid iff byte-identical to our chain at that
number; conflicts still halt). (8) debug_assert panicked when the FIXED code flipped a
persisted wrong Invalid (fix 63f6cfe: stale verdict from older binary — warn + trust the
certificate). ALSO: docker restart mid-height can hit malachite WAL "Round cannot go
backwards" crash-loop — heal = wipe <val>/malachite/wal/* (root container), node value-syncs back.
**ARCHITECTURE (user directive "robust EL, minimal CL, plug-and-play"): fork shim v1.1
(`50f0039` lean-prune): --peers, push-on-append gossip, SYNCING queue (512) + background
peer backfill; 3-node kill/restart gate converges with ZERO external feeding. CL = thin
4-verb client; contract in docs/lean-lane-integration.md (bc3ed09); validation-time
catch-up + CL peer-fetch fallback are CANDIDATE REMOVALS after v1.1 soaks.**
LANDMINE: CL image deploys need docker compose --force-recreate (docker restart keeps the
OLD image); after recreate re-connect arc_testnet_host-access + restart.

**🌙 OVERNIGHT CAMPAIGN 1 (2026-08-22, branch `lean-lane-integration`, commit `da922f2`): N-SWEEP
MEASURED + 4 LIVENESS BUGS FOUND & FIXED + 4h EVM-load soak clean.**
- **N-SWEEP (single box, 300M budget, governed feeders, all arms budget-full 97-100%):**
  N=1: 0.30 blk/s, 3,461 tx/s = 3,461 outputs/s, 11,538 txs/blk (exactly 300M/26k) ·
  N=10: 0.39 blk/s, 1,692 tx/s, **16,224 outputs/s**, 41,720 outs/blk ·
  N=50: 0.42 blk/s, 646 tx/s, **22,618 outputs/s**, 53,570 outs/blk. Fan-out delivers 6.5×
  outputs at 5.4× LESS admission; cadence IMPROVES with N (fewer sigs/output). Single-box
  cadence (0.3-0.42) is the only limiter → at fleet 2 blk/s, N=50 extrapolates ~107k outputs/s.
- **4 LIVENESS BUGS (all measured live, fixes in `da922f2`, 268 lib tests green):**
  (1) transient lean-node unreachability recorded INVALID; malachite's valid-round rule
  re-proposes certified values UN-revalidated → one ~10s restart = permanent 0-precommit
  deadlock (h3446). Fix: shim retries transport ~15s (30×500ms); past that return Err
  (no verdict), never Invalid. (2) lean catch-up only fired at decide; a lagging validator
  can't vote → no decide → no catch-up (2 validators 1 block behind = permanent 40/80-nil
  stall at h3499). Fix: validation-time peer catch-up before the verdict. (3) sync serving's
  offset mapping (head-(latest-h)) used a mid-recovery head → served wrong lean blocks →
  every height failed the cert ship-check → SKIPPED silently → syncing peer starved on empty
  responses. Fix: verify mapping against the height's certificate + scan ±128 for the true
  block; EVM-only heights (cert binds no lean) detected via commit_lanes(evm,None)==vid.
  (4) harness: ungoverned feeder blast (pool_target > the 51.2k slot cap) → eviction
  nonce-gaps → 40k queued-forever + tiny blocks. Fix: rate 2500/s, target 30k.
- **STAGGERED lean restarts under a LIVE chain now heal cleanly** (validated post-fix: 4
  sequential restarts, zero stall — the identical operation pre-fix wedged the chain).
- **4h SOAK (fixed image): 48/48 5-min slices healthy, 12/12 containers, perfect 4/4 lean
  lockstep, ZERO monitor heals.** CAVEAT: EVM lane carried its full 200 tx/s × 14,400s
  (2.88M txs) but lean spammers died ~15min in (-32000 = replacement-underpriced vs N-sweep
  pool leftovers at same nonces) → lean lane idle for most of the window. LEG 2 relaunched
  with pools wiped + a 2-min landing gate (52,638 lean txs/20 blks PASS).
- **OPS RULES BANKED:** (a) leftover pool txs poison the NEXT load phase (same-nonce
  replacements rejected underpriced) — wipe pools (staggered lean restarts) between phases;
  (b) SPAM_DUMP_FILE writes <path>.<PID> — merge before feeding; (c) never sample chain
  height from one node: val1 was 50+ behind while the chain moved (the value-sync-laggard
  landmine, again); (d) all-4-at-once lean restarts are the deadlock trigger — always stagger.
- **Self-healing monitor** (/tmp/overnight/monitor.py): 5-min checks, 30-min report lines,
  heals dead lean node (staggered) / lean lag>5 (peer-feed equalize) / EVM stall / lean
  freeze (equalize + rolling CL restart), each ≤1 per cooldown, all logged.
- **FORK DEEP PRUNE LANDED** (~/reth-fork `lean-prune` `4e6ee48`): 43→29 crates, 13M→6.7M,
  MDBX/C toolchain GONE — LeanProvider (~430 lines: basic_account + chain_spec real,
  trie/proof methods fail loudly) replaced MockEthProvider; storage stack survived only as
  dev-deps of vendored test suites. All gates green incl. byte-identical gen_vector.
- Idle cadence on the fresh chain read ~0.53 blk/s (below the 2/s pacer even unloaded) —
  UNINVESTIGATED, morning question.

**🎉 I4 FIRST LIGHT PASS (2026-08-22): 7,429 outputs/s THROUGH MALACHITE at just 743 tx/s
admission — 4/4 identical lean commitments, zero rejects (single box, N=10 smoke, 594k
outputs/80s).** Boot: lean-i4.sh scratchpad (demo-metamask + 4 host lean nodes --shim
--bind 0.0.0.0 + compose lean-override w/ per-CL ARC_PAYMENT_LEAN_RPC via docker gateway +
PEER_RPCS). THREE integration bugs found live, fixed same-day: (1) shim params array-wrapped
→ jsonrpsee positional parse → all builds failed, chain froze; (2) round-1 decides leave a
non-assembling validator with no bytes + value-sync never fires (consensus tip current) =
finding-#7 lean edition → decide-time PEER CATCH-UP (fetch gap from peer lean nodes until
commit_lanes(evm,head)==cert value_id; healed a 500-block outage in ~20s live); (3)
POISONED CACHE: sync dedup returned the store-loaded block — SSZ store drops lean bytes →
its value_id collapses to EVM hash → framework rejects vs cert FOREVER (diagnosed by
byte-logging both ends: frames identical; the cached copy lied). **CLASS RULE: any
store-loaded ConsensusBlock has a lying value_id in lean mode — reattach or rebuild.**
Cadence 0.56 blk/s under load on the single box (contention vs protocol TBD on fleet);
budget 300M (~42k outputs) not binding at ~13k-output blocks. NEXT: N∈{1,10,50} sweep,
restart legs, fleet.
**⚙️ LEAN-LANE INTEGRATION UNDERWAY (2026-08-22, branch `lean-lane-integration`, plan =
docs/lean-lane-integration.md):** wiring the lean lane node (~/reth-fork lean-lane-node)
behind Malachite as the payment EL, flag ARC_PAYMENT_LEAN_LANE (default off byte-identical).
DONE: I1 fork shim (arc_buildBlock/newBlock/getBlockBytes, --shim mode; 10k-block two-node
lockstep gate PASS incl. mid-stream kill/restart — first run caught the genesis-only-funding
rule; 1.22M fuzz cases 0 panics) · I2 arc types (LEAN_LANE_BIT=1<<62 framing, strict
decode_lean_block, LeanLanePayload, cross-pin test vs lean-node vectors byte-identical) ·
**COMMITMENT V2 (SECURITY, found writing the equivocation tests): txs_hash must bind the
FRAMED tx section (bytes[48..]), not the concatenated bodies — concat-only binding allowed
same-commitment divergent-state via boundary re-framing + total-STF no-ops (sync-peer
exploitable). Both sides adopted; regression tests both repos.** · I3a/b ConsensusBlock.
lean_payload + value_id via payment_lane_commitment + assembly/emission arms + wire
round-trip test. REMAINING: LeanShim client + get_value/decided/sync handler arms (I3),
single-machine 4-validator gate (I4), fleet number (I5). Lean gas metric: gas=21000+5000N,
budget env ARC_PAYMENT_LEAN_BUDGET_GAS, start 300M ≈ 42k outputs/blk for the 2 blk/s target
(measured knee in I4 sweep decides).
**🪤 CRATE-NAME LANDMINE (cost 8 masked errors): the local CL crate is `arc-node-consensus`
— `cargo check -p arc-malachitebft-app` silently checks the GIT malachite framework dep and
prints 'Finished' with zero work. All -p arc-malachitebft-app checks/tests this campaign
validated the wrong package (code was still compiled by docker workspace builds). Always
`-p arc-node-consensus`.**

**🧾 LAB NOTEBOOK WORKFLOW (2026-08-20, user-directed):** day-by-day experiment log with
figures lives at **`2026.arc.payment.highway.claude/lab-notebook.tex`** (append-only; one
section/day; every attempt gets PASS/FAIL/RETRACTED; figures only where they explain a
mechanism; push after compile-clean append). The deck (presentation/slides.tex) is now touched
only at milestones, cherry-picking matured notebook figures. docs/deferred-exec-100k.md stays
the dense text ledger; the notebook is its illustrated twin.

**🔌 ISOLATED INGRESS — builder as the admission layer (2026-08-20..21, branch
`builder-separation`). ARCHITECTURE VALIDATED IN PIECES; WAIT-FIX IMAGE NEVER REACHED THE
FLEET (torn down mid-ship).** Design: ALL spam → ONE unpeered builder EL (`builder-el.sh`
with `PEERS='' BUILDER_EXTRA_ARGS='--disable-tx-gossip' BUILDER_DEADLINE=150` + datadir
wipe), every CL gets `ARC_PAYMENT_BUILDER_ENGINE/ETH_RPC` + `ARC_PAYMENT_DEFERRED_EXEC=1`
via CL_EXTRA_ENV; validators' pools stay 0 (zero admission tax on voters — txs travel only
inside proposals); load = laptop `ingress-daemon.py` (closed-loop, `--skip-files` for corpus
reuse). MEASURED: 998k txs ingested by the builder ALL mined, zero loss; every HIT = a
perfectly FULL 7,142-tx 150M block; agreement OK at every check; chain 1.95 blk/s @500ms
after val3 recovery. Rationale: sustained = min(capacity, admission); admission (~10k
ceiling, in-EL contention) was the binding term — this moves it off the voters.
- **THREE REAL CL BUGS found+fixed in the prebuild path** (invisible before isolation:
  gossip-filled pools made a miss a FULL local block; isolation makes it EMPTY):
  (1) stale-confirm burn — refresher polls ≤2s for a fed head the builder already moved
  past; a fresh trigger queues behind the burn past get_value (hits 0-40% = phase luck).
  Fix `0f5a43c`: trigger carries block number, bail when builder head > want, clear failed
  expected. (2) serial dual-ts builds (2×150ms deadline) = ~550ms cycle vs 512ms height →
  0% hits at 2 blk/s (22% at degraded ~1.5s cadence). Fix `88b2780`: build t0/t0+1
  concurrently. (3) THE DEEP ONE: under DEFERRED exec get_value fires ~30ms after decide
  (structural validation is instant) and always beat the ~350ms refresh; its `mem::take`
  also drained future candidates on every miss. **v1.1's hit rates only ever worked because
  GATED execution delayed the next height.** Fix `fc10d3c`: bounded 450ms wait-for-stash in
  get_value (strictly better than the 500ms-deadline local build it replaces), remove only
  the matched entry, refresher logs → info (runs are now self-diagnosing).
- **🎯 GATE RUN PASSED (2026-08-21 evening, all 3 fixes deployed fleet-wide):** 150M,
  500ms pacer, deferred + builder-on-GINNY (unpeered, --disable-tx-gossip, deadline 150,
  wiped datadir), laptop daemon 20k offered, measured from uninvolved val2: **9,149/9,058/
  8,212 tps landed over three 160s slices, 76% of blocks perfectly FULL (7,142 tx),
  cadence 1.61-1.70 blk/s, entire 6.18M-tx corpus accepted zero rejects (~8.8k/s accepted
  WHILE consenting), agreement OK, builder peers 0 start+end.** Hit rates: val1 89% /
  val2 ~91% / val3 ~81% / val4-wifi ~25% (the fullness leak). vs the 0.5-0.63 blk/s
  collapse at 4k direct ingress: **sustained landed went 3.7k → 9.1k tps.** Gaps to the
  14k ceiling (7,142×2/s): val4's hit rate + cadence 1.7 vs 2.0. NEXT: straggler fix or
  3-wired-validator arm; 300M; then the production raw-tx forwarder (WS1).
  **300M probe (same night): landed stuck at 8.1-8.6k (= 150M) w/ 42-44%% full — NOT chain-bound:
  2×1.75MB stash fetches over the builder's wifi blow the 450ms get_value wait (val3/4 hits 21/16%%);
  sharding the daemon to 16 feeders made it WORSE (val1 84→66%%, landed declining to 3.2k) =
  in-EL pool-lock contention recreated ON the builder. 300M needs wired builder and/or compact
  stash fetch. Chain restored to 150M. daemon --shards exists but is measured-negative on a
  single builder.**
  Boot recipe that finally worked: iso7*.sh scratchpad pattern = ship-images + CONTAINER
  CENSUS per host (val3's tar-ship failed silently AGAIN — census gate is now mandatory) +
  val3 on papaduck HOME disk (NVMe mount has a hung-rm pathology) + builder unpeer check
  before the window (val1_el_pay's *_el_pay glob swept the builder in once more) + laptop
  tools re-ship (machine restarts wiped /tmp on BOTH boxes).
- **🎯 arc_rawPayload (SSZ+gzip prebuild fetch) MEASURED POSITIVE at 300M (2026-08-21 night):**
  no reth fork — EL serves base64(gzip(ssz(V3))) via extend_rpc_modules
  (crates/node/src/raw_payload_rpc.rs, PayloadStore, same resolve semantics as getPayload);
  CL generate_block tries it under ARC_BUILDER_RAW_FETCH=1 w/ engine-API fallback; decode
  corruption is caught by the deferred structural gate (fail-safe trial). 300M A/B same
  topology: hits val2 59→~74%, val3 21→~52%, val4 16→~52% (val1 local 84→85%), landed
  8.1-8.6k → 8.6-9.6k tps, cadence 1.3-1.4 → 1.5-1.6, 6.39M txs zero rejects, agreement OK.
  RESIDUAL at 300M: remote misses 25-50% (wifi still marginal at ~2MB fetches) + the
  4-thread daemon's ~9k/s delivery ceiling. GOTCHAS: demo-fleet's val3 tar-ship failed
  3-for-3 after papaduck's reboot (targets the NVMe path; scripted home-disk repair =
  scratchpad v3fix-resume.sh — FOLD INTO demo-fleet-metamask's val3 leg); val1_el_pay's
  *_el_pay glob re-swept the builder into its mesh at every launch (unpeer step is part of
  the pipeline now; harmless mid-run given --disable-tx-gossip).
- **MULTI-SOURCE + DEADLINE ARMS (2026-08-22 early):** 3-machine gentle ingress (laptop 12k +
  ginnythui 6k + alien2 6k caps): landed ~9k with builder pool PINNED at 90k target the whole
  window → **sustained = mining = cadence × prebuild fullness; delivery was NEVER the limit**
  (daemon-ceiling theory falsified). Gentler streams RAISED hits (90/80/67/55%). Avg hit-block
  ~6.1k/14,285 txs ≈ a 150ms build's packing → tried BUILDER_DEADLINE 300ms: WORSE (landed
  3.5-7.5k, fullness 14-33%, hits unchanged) — under concurrent admission the build iterator
  crawls; longer deadline = more crawling, not more txs (the 6-10× build-ingress inflation).
  REVERTED to 150ms. **Standing sustained record: 9.0-9.6k @300M** (raw fetch + multi-source).
  20k goes through: wired builder link, lean/FIFO pool (WS4), fan-out type — not knobs.
- **SPAMMER IS LEAN-READY (2026-08-22, arc `b7dcff0` + fork `5b86994`):** `--mix fanout=N` +
  `--fanout-outputs K` emits 0x50 fan-out txs; codec mirrored in crates/spammer/src/lean.rs
  with a BYTE-EXACT pinned vector test vs the fork (`gen_vector` example — breaks FIRST if
  either side drifts); SpamTx enum carries lean raw bytes through the envelope pipeline
  (dump/corpus mode works). END-TO-END SMOKE vs lean-lane-node: 31.9k txs = 21.3k outputs/s
  over ONE ws thread, 0 failures, 59 blocks. Hookup gaps found+fixed by the smoke: node
  lacked eth_getTransactionCount; run-mode had no genesis funding (--fund-file/--fund-balance
  added, snapshotted per the recovery gotcha); SPAMMER DERIVATION IS m/44'/60'/**1'**/0/i (not
  the standard 0' — fund files must use it); fund enough for wei-denominated pool fee math
  (fee/tx = (21000+5000N) gwei). FALSE-POSITIVE LADDER (bank it): spammer's error tracker
  truncates server reasons to [0xhash]; timeout-kill loses the buffered dump file; the status
  ticker keeps printing after the sender task dies — smoke with clean exits + unfiltered logs.
- **🎯 LEAN LANE NODE (increment 2b', agent, ~/reth-fork `29179e3`): 687,056 outputs/s
  sustained (18.6k tx/s @ avg N=37), INGRESS-BOUND — single-threaded loop ceiling ~600k+.**
  "reth as a mempool library": stock 2a pool unmodified + fan-out total-STF executor on a
  flat map + append-only log w/ snapshots + derived receipts + arc_sendRawTxBatch (23k tx/s
  offered through it). NO header/roots: commitment = keccak(parent‖number‖ts‖txs-hash).
  Product config 150M/250ms: 105,762 outputs/s budget-bound, ~1.6µs/output. Per-stage vs
  ledger: admission 1.08µs/out (33µs/tx ecrecover), execute 0.08-0.13 (7-13µs/tx), append+
  fsync 0.04-0.13 (44µs/tx persist), log 30.1B/out (122B). Recovery 2.22M outputs/2.5s
  digest-verified; crash gates green. 23 tests, workspace check green. CL shim SPECIFIED
  not built (buildBlock/newBlock/getHead + value_id binding + deferred composition —
  LEAN-NATIVE.md). Caveats: standalone/no consensus, one box, avg-N=37.
- **LEAN FAN-OUT INCREMENT 2a DONE (agent, ~/reth-fork `lean-native-transfer` c6a54ad):**
  wire v2 = 72+28N (u64 gwei amounts; type byte 0x50 carries version): **35.2 B/output @N=10
  (3.47×), 28.7 @N=100 (4.25×)**; canonical bytes = 2718 bytes, decode-once w/ retained raw
  (later encodes = memcpy; RLP = passthrough); **ZERO reth source edits** (stock pool
  `with_custom_tx_type(0x50)`); 21 tests through real Pool incl. rejects; full workspace
  check green; fee = (21000+5000N)×1gwei. 0.9MB block ≈ 25.6k outputs @N=10. 2b (execution+
  receipts) deliberately deferred. ~/reth-fork/CLAUDE.md is current for fresh sessions.
- **LEAN NATIVE-TRANSFER TYPE — INCREMENT 1 DONE (2026-08-21, agent, ~/reth-fork branch
  `lean-native-transfer` commit `9ab4a08`, arc repo untouched):** fan-out native transfer
  (1 sig → N recipients), fixed-width no-RLP codec + serial/parallel executors +
  differential tests (10 green; serial==parallel on pooled/hot/duplicate/self/failure) +
  bench. MEASURED (this box): N=1 108B/4.4µs-per-output; **N=10 43.2B + 0.47µs/output
  (2.82× packing, ~70× under the 33µs/tx serial baseline)**; N=100 36.7B + 0.09µs.
  Semantics locked: whole-tx atomic; same-block incoming credits NOT spendable (keeps
  sender-partitioned parallel deterministic); duplicate recipients sum; zero-value = no log
  (bug-ledger #3 as a test); fee = one deferred end-of-block beneficiary credit; lane-domain
  in the sig hash (no chainId on wire). Increment 2 touchpoints (verified paths) in
  ~/reth-fork/LEAN-NATIVE.md: envelope crates/ethereum/primitives, pool validate/eth.rs,
  payload builder, engine-tree execute_transactions (~L1236, BatchExecute pattern), receipt
  derivation. A 0.9MB block ≈ 20k outputs at N=10 vs 7,142 today.
- **GOTCHAS BANKED:** wipe the builder datadir before relaunch (stale node key → validators'
  persisted known-peers redial it — observed twice); NEVER stack rm -rf on a chain datadir
  (papaduck "freeze #2" = 3 concurrent deleters + 2 tars in D-state on one ~100GB tree;
  disk was healthy, 2.1ms fsync); val3 now boots from /home/papaduck/arc-fleet (home disk
  2.3ms fsync) after the NVMe-mount incident; demo-fleet's per-host tar ship can fail
  SILENTLY after a remote reboot (val3 booted with NO CL container — census the containers
  after every boot); value-sync healed 3,000 heights in ~6min; an empty un-paced chain runs
  ~14 blk/s and permanently outruns the builder — ALWAYS pace isolated-ingress runs;
  local val1_el_pay swept the builder into its addPeer mesh once (`*_el_pay` name glob —
  builder container name matches; remove peer both sides).
- **PRODUCTION SHAPE (user-endorsed direction):** the daemon is a stand-in for a real
  raw-tx FORWARDER in the node (op-reth `--rollup.sequencer-http` pattern): users submit to
  any validator RPC, the node forwards to the builder instead of pooling+gossiping. Build it
  once the hit-rate gate passes — "the forwarder, not a better spammer."

**🔩 RUNG 4 (compact proposals v2) + BUILDER v2.x STATUS (2026-08-19..20, branch
`builder-separation`):**
- **Compact v2 = v1 + the getblocktxn-style fallback:** on reconstruction miss, batch-fetch the
  missing txs from the PROPOSER's payment EL (hash-verified per tx; proposer provably holds all
  it packed), keyed by `ARC_PAYMENT_PEER_RPCS` (JSON: consensus addr lowercase-hex -> pay-EL eth
  URL; local demo builds it from CL log line 'local signer identity address='). Local-EL
  transport errors degrade to all-missing -> fallback (a restarted EL erred out BEFORE the
  fallback on first live test). Emission still `ARC_COMPACT_PAYMENT_PROPOSALS=1` default-off.
  Local validation: phase-1 healthy pools PASS (1.87 blk/s, agreement, 0 fallbacks needed);
  phase-2 forced-divergence (restart a pay EL = wiped pool, the exact v1 killer) = the gate.
- **Builder v2.2 (continuous, event-driven):** decide-time single-feeder hands the refresher the
  exact fed head + wakes via Notify; refresher confirms head then builds t0/t0+1; empty-candidate
  guard (starved builder must not win a hit with a hollow payload). Mechanics verified live;
  SCORING PARKED on link: wifi cannot carry ~12MB/height (papaduck-extreme = wifi-only, 16c/30G/
  0.61ms-fsync NVMe — best disk in fleet; needs ethernet OR compact-v2 lightening the link ~4x).
  WIFI LESSON LADDER (each cost a fleet run): 4x-feed 27% hits; v2.0 all-CL refresher fetch storm
  (~50MB/s, chain 3x slower); v2.1 polling loses the decide->get_value race (0/1,077); v1.1 wired
  co-located = +9.4%/+17.6% stands as the rung's measured result.
- **Fast-harness facts:** per-arm 8-10 min (corpus fills); reth admission = 36-41k tx/s batched
  raw (the old '5-8k delivery ceiling' was the ws-ack spammer, NOT reth); 1G depth penalty
  (26.1k@203k / 25.2k@250k(7blks) / 17.6k@313k — operate at 250k); corpus fill ceiling was
  senders x slots (FILL_ACCTS); cap overshoot strands queued successors FOREVER (pending-only
  quarter-step fills); bench-drain needs a full at-size block before honoring the sub-full end
  condition (post-flip transition block); sampling from a value-sync laggard fabricates tps
  (54-57k 'bursts') — always tip-convergence-gate the measurement source after CL restarts.

## 🐛 BUG LEDGER (consolidated, 2026-08-10 — details in the dated entries + MISSION-EL.md)

**Consensus-critical (all in the gated-off native-transfer fast path; all fixed; ONE root shape:
"assume what a tx is instead of asking it"; 3 of 4 invisible to state-only comparison):**
1. Missing per-transfer log — fast path emitted no EIP-7708/NativeCoinTransferred log → receipts
   root forked vs stock peers. Caught only by A/B vs UNMODIFIED peers.
2. Legacy-tx fee shape — hardcoded 1559 formula collapsed to basefee for type-0 txs → state root
   forked (receipts matched!). Fix: tx.effective_gas_price(basefee).
3. Zero-value transfers — interpreter emits NOTHING when value==0; fast path emitted a log →
   receipts diverged. Offline-gate catch (Workload::Mixed).
4. (Related retraction, not a fork: "eager recovery 4x" was metric relocation — only
   new_payload_latency counts.)
**CL robustness (fixed unless noted):** proposer crash-exit on payment getPayload timeout (fix:
skip round, 509f404); value-sync batch livelock (env-tunable batch/timeout); OPEN finding #7:
ProcessSyncedValue treats newPayload=Syncing as fatal → CL crash-loop after pay-EL OOM (ops heal
scripted in revive-val.sh; code fix pending, CL-side so out of mission scope).
**Infra/observability (fixed 2026-08-10, PR patches in /tmp/pprof-fixes + branch
`pprof-heap-profiling-fixes`, ported to fleet-multi-machine + payment-lane-gas):**
5. pprof bug 1 — malloc_conf exported unprefixed; tikv jemalloc reads _rjem_malloc_conf →
   opt.prof always false (b4df3e3).
6. pprof bug 2 — --pprof.heap-prof activated via raw mallctl, bypassing jemalloc_pprof
   PROF_CTL's cached bookkeeping → /debug/pprof/allocs always closed with an empty reply
   (c43527e). Together: the feature was dead-on-arrival; env workaround
   _RJEM_MALLOC_CONF=prof:true,prof_active:true.
**NOT bugs (design-meets-unusual-scale, fixed by config):** RPC eth cache is ENTRY-bounded
(5000 blocks) — gigabytes at payment block sizes = the "unbounded" memory growth; capped
200/200 by default in all launchers. Pool-cap eviction collapse under over-offered load =
open-loop spamming problem; fixed by the closed-loop governor (--pool-target).
**Ops landmines (each cost a run):** pkill/pgrep -x fails silently on >15-char process names;
pgrep -f matches its own shell; tailscale-ssh check expiry no-ops remote commands while exit
codes lie; internal docker networks have no host route; anchored doc edits need
`assert anchor in s`; smoke the VERBATIM harness arg string after any binary rebuild.

## 📌 PAYMENT-LANE STATUS SNAPSHOT (2026-08-16) — where everything stands

**Branch `payment-el-perf`** (renamed from `blockstm-native-transfers` — Block-STM never
implemented here; the verified parallel scheme is sender-partitioned, offline bench only).
Repo clean, everything committed; all 4 machines torn down and REMOTELY VERIFIED clean
(0 validator containers, 0 spammers, no chain datadirs; ginnythui's 18 containers are the
user's personal services — "arc" grep false-positives on tubearchivist/archivist-redis).

**MISSION 4 VERDICT (all levers measured, none re-openable without new information):** the EL
is done as a lever (8-15% of height; exec 3.8ms/blk at the 2 blk/s point). Fast path ≈ +0.5%
height (gated OFF); eager recovery = metric relocation (reverted); prebuild = -8% drain @60M /
+2.8% sustained @150M (hit-starved; ~13% prize gated on pending-visibility fork work — PARKED);
persistence/cache/builder knobs + cpuset = null. Remaining bottleneck = consensus coordination
(~49µs/tx: slowest-peer re-exec + transport + votes) — CL-side, out of scope. Priced-but-parked:
pool-lock/snapshot-iterator fork (~15-20% sustained), pending-visibility fork (~10%), CL compact
blocks / pipelining / erasure coding.

**CANONICAL CAPACITY TABLE (2026-08-12/13, drain-campaign.sh, deep healthy pools; REPRODUCED
n=2 on a second fresh chain within ±6%, 30M to the millisecond; drains cross-validator-verified
(15/15 hash+stateRoot identical val1-vs-remote-val2) + latencies cross-checked against on-chain
timestamps):**
| gas | ms/blk | tps | n |
|-----|--------|-----|---|
| 30M | 138 | 10.4k | 2 |
| 60M | 238-252 | 11.3-12.0k | 2 |
| 100M | 344-363 | 13.1-13.8k | 2 |
| 150M | 436-451 | ~16.1k | 3 (+1 straggler outlier 621/11.5k on record) |
| 200M | 573 | 16.6k | 1 |
| 300M | 744-845 | 17.6-19.2k (median 773/18.5k) | 6 |
| 500M | 1.39s | 17.2k (spread 16.8-20.9k) | 4 |
| 1G | ~2.2s | ~21k* | 1 (*3 blocks, resolution-limited) |
Fit ≈ **70ms + 49µs/tx** (asymptote ~20k). **Plateau 16.6-19.2k from 150M; past 300M only
latency grows** (300M→1G: nil tps, ×2.9 latency). Sustained (governed, live ingress): 6.6k@30M
→ 11.0k@300M fleet grid; demo-chain 200M sustained reproduced 5×: 7.8-9.7k avg / 9.4-12.2k peak.
Old ≤2026-08-11 drain numbers (14.2-14.3k etc.) = byte-cap-pinned pool regime, uniformly 12-24%
slower at every size — SUPERSEDED; do not quote.

**TOOLING (committed):** `drain-campaign.sh` = one-command deterministic capacity campaign
(JSONL rows carry backlog + chain age; fresh chain per campaign — pool-tracker leak degrades
fill ceilings over ~5-8 mass-drain cycles); `run-bench.sh` + helpers = dashboard one-click bench
(governed sustained window + capacity drain; trap-restored governance flips, header-verified);
pool caps trio now launcher defaults (slots 256 / 512MB sub-pool sizes / 200k counts — all
three needed, each was a separate fill ceiling). Slides: every capacity frame reads from the
one canonical campaign (paper repo 62d3815); slide-14-vs-16 contradiction resolved.

**PLATFORM FACTS from the campaign week:** targetBlockTimeMs CL-clamped to [0,1s] (silent reset
to 500ms); reth per-sender slot cap 16 → pool ceiling = senders×16; reth sub-pool byte caps
~20MB ≈ 40-70k transfers; reth pool size-tracker LEAKS under repeated mass-drain churn
("txpool is full" at status 0, single txs still accepted — restart clears); governance txs
starve behind backlogs unless high-tip. Spam tx = type-2 EIP-1559 native transfer, 120-123 B
signed (measured offline via cast, matches live 122 B avg), gasLimit 30k / gasUsed 21k.

**🔬 CANONICAL DRAIN CAMPAIGN + reth POOL-TRACKER LEAK (2026-08-12).** drain-campaign.sh
(committed) = deterministic one-command capacity drains (JSONL rows carry fill_target/backlog/
chain_age). Results, healthy pools (stop-time backlogs >=100k): **150M 437-451ms/15.8-16.3k (n=2;
one 621ms/11.5k straggler outlier on record) · 300M 744-811ms/17.6-19.2k (n=5, tight)**; capacity
is DEPTH-INSENSITIVE across 100-192k stop-backlogs. RECONCILIATION of the old 14.2-14.3k campaign:
its pools ran PINNED at the 20MB byte cap (fills "48-73k" = the cap) with eviction nonce-gapping —
a degraded-pool regime + 3-4-block drains; today's healthy-pool numbers supersede (+13% @150M,
+30% @300M). A true-shallow arm is IMPOSSIBLE by construction (spam feeds through the flip
transition: a "53k" fill = 10 full 300M blocks by stop). **NEW PROBABLE reth BUG: pool
size-tracker LEAKS under repeated mass-drain churn** — fill ceilings degraded monotonically over
8 cycles (192k→138k→53k→wedged at 48k), "txpool is full" storms while txpool_status shows room
and SINGLE txs still land (acceptance probe passes; bulk rejected). Restart clears it. RULE: one
chain supports a limited number of mass-drain cycles — fresh chain per campaign; declining
backlog ceilings in the JSONL = re-boot. CANONICAL TABLE COMPLETE (fresh chain, all healthy pools): **30M 138ms/10.4k (139 blks) ·
60M 252/11.3k (70) · 100M 344/13.8k (41) · 150M 444/16.1k (n=2) · 200M 573/16.6k · 300M
773/18.5k (n=5) · 500M 1,420/16.8k · 1G ~2,227/~21k (3 blks, resolution-limited)**. New fit
~70ms + ~49µs/tx (asymptote ~20k); plateau measured 16.6-19.2k from 150M; 300M→1G = nothing but
latency ×2.9. New campaign is 12-24% faster than old AT EVERY SIZE → the whole old campaign was
byte-cap-regime-depressed, fully superseded. Slides rebuilt from this one table (68fe4be):
finalize-bars totals, TLDR capacity series, plateau frame — slide-14-vs-16 contradiction
RESOLVED. Reproduce: SIZES="30 60 100 150 300" FILL_TARGET=190000 drain-campaign.sh on a FRESH
fleet chain. ALSO: stale self-matching watcher shells from old sessions can block pgrep-gated
automation for DAYS — patterns match fossil cmdlines; purge with ps -eo lstart before trusting
any pgrep gate.

**🎯 1-GGAS DEEP-DRAIN SWEEP — PLATEAU MEASURED, KNEE AT 200-300M (2026-08-12).** One chain,
one day, ~190k prefill per size, intake stopped, only 100%-full target-size blocks counted:
**200M 16,615tps@573ms (20 blks) · 300M 17,605@811 (12) · 500M 16,773@1,420 (7) · 1G ~21k@2,227
(3 blks — resolution-limited, pool holds only 4; NOT evidence of growth)**. The fitted ~17k
asymptote is now OBSERVED: tps flat 200M→1G while latency ×3.9. Deep pools (slots 256 + 512MB
size caps) lift ceilings ~15-20% vs the ≤73k-backlog campaign (old 300M 14.2k → 17.6k) — same
shape, higher plateau; quote the deep-drain numbers for capacity. Slides updated (new frame after
TLDR, pushed 6fbe8de). HARNESS BUGS FIXED EN ROUTE: (1) bench-drain early-exit on wrong-size
transition blocks killed the first 1G run mid-drain (its 2 blocks WERE 100% full on-chain);
(2) trap restores must send HIGH-TIP governance (unpriced restore starved behind a 95k backlog,
left chain at 25M); (3) after churn the pool can reject with "txpool is full" while txpool_status
reads 0 (residual sub-pool accounting, self-clears ~minutes) — drain harness now probes with a
real tx before filling. PROCESS LANDMINE (cost two runs tonight): capture PIDs from ps AFTER
setsid (it re-forks; watching the wrapper PID = premature "done" → I stomped a live run with a
manual restore). Sweep harness: scratchpad drain-size.sh (session-local).

**✅ CAPACITY DRAIN VERIFIED END-TO-END ON THE FLEET (2026-08-11).** Third pool-cap layer found:
per-sub-pool BYTE caps (~20MB ≈ 40-70k transfers; fill hit 70k in 12s then eviction collapsed it
to 5k). Launchers now raise all three layers (counts 200k + slots 256 + pending/queued/basefee
max-size 512MB). Verified live on a fresh 4-machine fleet: fill 165,643 in ~24s; **drain @ 200M =
16,615 tps @ 573ms, 20 consecutive 100%-full 9,523-tx blocks (capacity_gas_m=200 on the card)**;
sustained same run 9,435 avg / 12,216 peak (91%% full); chain restored 200M/500ms/2.00 blk/s.
Card now shows capacity ~1.7x above sustained — the correct story.

**🔧 DASHBOARD ONE-CLICK BENCH REBUILT (2026-08-11) + two platform facts.** run-bench.sh =
governed sustained window + prefill/drain capacity phase (bench-pool-wait.py, bench-drain.py);
9 test fires on the live demo chain. NEW FACTS: (1) **targetBlockTimeMs is CL-bounded to
[0,1s]** (consensus_params.rs enforce_bounds resets out-of-range to 500ms) — the pacer can
NEVER throttle consumption for pool-filling; the working fill throttle is the runtime GAS-LIMIT
shrink (25M) + high-tip restore under load (governance tx must outbid the backlog; strip cast's
"[2e10]" annotations when re-sending feeParams). (2) **reth --txpool.max-account-slots defaults
16/sender** → pool ceiling = senders×16 (800 accts = 12.8k, measured 4×) — a 200M drain needs
~50k+, so launchers now pass --txpool.max-account-slots=${PAY_ACCOUNT_SLOTS:-256} (fresh chains
fill 150k+). run-bench skips the drain below 30k backlog with a card note (small-block capacity
reads BELOW sustained = confusing); stop window bleeds ~8k txs (parallel pkill ~2s). Sustained
on the demo chain reproduced 5×: 7.8-8.3k avg / 9.4-10.1k peak @ 200M 54-58%% full
(delivery-bound). Dashboard stress button fixed (S=1 ACCTS=200 — the 16k-account assumption).
All governance flips trap-restored + verified (200M / 500ms / 2.00 blk/s) after every fire.

**🧪 CPUSET A/B (2026-08-11): decisive null — pinning pay ELs to HALF their cores away from
spammers/CL changed nothing (742ms/9.6k both arms; builds ±5-7%).** The 6-10× build-ingress
inflation is IN-PROCESS pool-lock/iterator contention, not CPU scheduling; the EL isn't even
CPU-starved at half cores. Closing the drain-vs-sustained gap needs pool-internal work (sharded
locks/snapshot iterator, fork) or process-separated admission — not ops tuning. Contention
story complete: measured, localized, decomposed, remediation priced.
**⚖️ SUSTAINED PREBUILD A/B @150M (2026-08-11): +2.8% tps only (709/10,074 vs 729/9,802) —
hit-starved: miss_parent 69-101 vs hits 3-35 per validator.** The chain-anchored publish fix
(91-100% hits @60M) fails at 150M sustained (heavier newPayload(N) races FCU(N-1)). The
ledger-implied ~13% is real (live builds 80-140ms) but gated on deeper fork pending-visibility
work — OPEN, not pursued. Flag stays default-OFF; -8%@60M capacity remains the quotable win.
**🔬 PER-STAGE LEDGER (2026-08-11, 150M both regimes):** (1) **build is where ingress lands:
96-143ms sustained vs 10-18ms quiet-pool for identical blocks (6-10×, pool-lock/iterator
contention)** → prebuild's sustained value likely ~13% not the drain-A/B's -8%; sustained
prebuild A/B is the justified next experiment. (2) exec discrepancy resolved: per-tx
executor+recovery is regime-independent (~7-13µs/tx); old 20µs "EL slope" absorbed contention.
(3) finalize-bars EL segments were empty-block-diluted at big sizes — corrected to per-tx×n
(consensus share 85-93%). GOTCHA: drain-window prom deltas include post-drain racing EMPTY
blocks — always compute EL as per-tx × n, never ms/blk over a drain window.
**📏📏 REPEAT CAMPAIGN (2026-08-10/11): all five sizes re-measured, EL captured on the same
blocks — everything reproduces across campaigns days apart** (30M 181 · 60M 293 · 100M 401 ·
150M 501/14.3k · 300M 1,003ms; medians of n=5-6 drains). **Consensus = 93-96% of finalize time
at every size; EL 13-41ms.** Straggler episodes (occasional 1s stall blocks: 265ms@30M,
638ms@100M drains) excluded from medians, kept on record. Slide 13 = stacked EL/consensus bars;
slide 15 = 3-regime plot w/ legend below. 300M drains structurally thin (50k backlog ≈ 3.5 blk).
**📏 LAW SWEEP (2026-08-10, user-directed): the law is MEASURED — 5 sizes × 3 fresh-chain
drains, refit latency(n)=110ms+58.7µs/tx, residuals ≤7% over 10× (30M 181ms/7.9k · 60M 295/9.7k
· 100M 407/11.7k · 150M 501/**14.3k** · 300M ~0.91-1.0s pooled/14.2-15.7k). Ceiling now measured:
14.3k@150M = 84% of the 17k asymptote; 150→300M doubles latency for ~0-10% tps (capacity knee
~150M). 2blk/s crossing → ~139M gas. ≥150M drains are resolution-limited (4-8 blocks vs 44-53 @
30M; pool/median, note 1s sampling). Consensus slope = transport+decode(~18µs) + slowest-peer
re-exec(~20µs); votes hash-sized ≈ 0.
**✅✅ FLEET ENDURANCE SOAK PASSES (2026-08-10): 75 min at 200M@1s/9.2k tps, 15/15 slices
healthy, agreement OK throughout + final 20-block check, all four validators alive** (pre-fix:
v4 dead at slice 6). Memory converging sawtooth → late plateau: final v1 4.4 / v2 5.1 / v3 7.5 /
v4 4.1 GiB, tail rate ~0-15 MiB/min (vs ~250 pre-fix); fleet plateaus higher+later than local
(load, pools, per-machine baselines). Endurance chapter CLOSED with fleet receipts.
**✅ ENDURANCE FIX VALIDATED AT THE OPERATING POINT (iters 28-30, 2026-08-10).** 75-min
single-machine soak, 200M@1s, ~7.9k tps, caps 200/200: chain healthy ALL 15 slices (agreement OK,
98-100% full), **memory plateaus at ~3.2 GiB on all four pay ELs (flat last 5 slices)** — the
iter-27 "residual growth" was the cache still filling; converged residual ≈ 0. Plateau scales
with block size (1.7 GiB @100M, 3.2 @200M). GOTCHA fixed on both branches: fleet
demo-fleet-metamask.sh has its OWN pay-EL docker run — launch-payment-els.sh edits don't reach
it; caps added there too. Fleet soak re-run pending only tailscale-ssh re-auth (check-mode
expires ~12h; every remote step silently no-ops with "Tailscale SSH requires an additional
check" — VERIFY remote effects, the ssh exit code lies).
**🎯 ENDURANCE SOLVED — RPC-CACHE CAPS (iters 26-27, 2026-08-10).** Heap-profiled the growth
(had to FIX Arc's pprof first — dead-on-arrival from 2 bugs: malloc_conf unprefixed vs _rjem_
prefix b4df3e3; activation bypassing jemalloc_pprof PROF_CTL bookkeeping c43527e; workaround
_RJEM_MALLOC_CONF=prof:true,prof_active:true; profiles need `--profile profiling` build —
release is stripped): **51% of in-use heap = reth's RPC eth cache** (ChainChange::new clones
every block+receipts into ENTRY-bounded LRUs, 5000/2000 defaults = GBs at payment block sizes,
never fills → the "unbounded" growth + idle release). A/B `--rpc-cache.max-blocks 200
--rpc-cache.max-receipts 200`: **val1 FLAT at 1.7GiB from min 13; stock peers 5.2GiB+ still
climbing; agreement OK.** 27-min OOM bound GONE; "no config knob moves it" SUPERSEDED (the knob
was never in the earlier tests). launch-payment-els.sh now defaults PAY_RPC_CACHE_BLOCKS/
RECEIPTS=200/200. Landmines: internal docker net has no host route (probe for the 404, not TCP);
`pkill -x` silently fails on >15-char names (kernel comm truncation).
**🔬 MEMORY GROWTH = REAL ANON HEAP (iter 23, 2026-08-10).** cgroup memory.stat attribution under
governed load: anon climbs monotonically (~0.24GiB/min at ~5-6k tps) while file/MDBX-page-cache is
squeezed 3.0→0.4GiB as the reclaim buffer once the cap pins. OOM = anon exhausting the cap after
file is gone → docker/cgroup tuning CANNOT fix it (and no reth knob moves it, prior result).
Component attribution needs a jemalloc-profiling build (reth_jemalloc_* gauges absent in this
build) — recorded next step, not pursued.
**🕐 OPERATING-POINT SOAK (iter 21, 2026-08-10): 200M@1s/9.2k is stable until memory, then
degrades predictably.** 75-min governed soak, 5-min slices w/ agreement+memory: healthy
1,026-1,053ms/9.0-9.3k (slices 1-5); **v4/alien2 (11GiB cap) OOM'd at ~27min** (all ELs grow
~0.4-0.6GiB/min at 9k tps, linear, no plateau — v3 hit 21.5GiB on 78GB, fine); then **3-of-4
degraded steady state 1.85s/~5.1k flat** — a dead validator costs ~45% (round-robin slots burn
full timeouts), not 25%. OPS: small-RAM validators need EL restart cadence (~25min@11GiB) or a
lower sustained rate; run mem-soak alongside anything >30min.
**✅ PACED SWEEP REPRODUCED n=2 (iter 20, 2026-08-10, user-directed, remote-spam-verified):**
operating points within 1% both days — 2 blk/s→50M/4.6k HELD, 1s→200M/9.2k HELD, 100M unpaced
8.65-8.86k. **300M REQUALIFIED to a range: 8.2-10.6k @ 1.35-1.74s** (last-in-sequence both days;
today's window suspect — val RPC dead at end (alien2 OOM window), agreement check didn't complete
(no fork: production never stalled); yesterday's had verified agreement). Don't quote one 300M
number. GOTCHA: remote spam binary is /tmp/arc-spammer → `pgrep -x spammer`=0 on remotes; census
via pgrep -f arc-spammer or spam-log mtimes. Forensics confirmed yesterday's sweep was fleet-fed
(log mtimes match every window; tailscale expired overnight, after it).
**🎯 PACED SWEEP (iter 19, 2026-08-10): 1s HEARTBEAT HOLDS 200M/9,206 tps; sustained record
10,595 @ 1.35s (300M).** Governed spam, one chain, runtime gas+pacer flips, all points 100% full
queued=0: 500ms→50M/4,591 HELD (100M 580ms/8,213 + 130M 659ms/9,388 miss — sustained 500ms
crossover is ~50-60M, not the zero-ingress 132M); 1000ms→100M/4,761 exact + 200M/9,206 HELD +
300M 1,348ms/10,595 miss. Paced points parallel the drain frontier; gap = concurrent-ingress cost
(~1.3-1.6x at 100M+). Chart: `experiments/dual-el/paced-tradeoff-chart.py`; deck has load-technique
+ pick-a-heartbeat frames; LOADING.md (both branches) documents the technique; governor+flags
ported to fleet-multi-machine (cf6c530 — that branch's spammer source now self-contained;
previously its scripts only worked with a payment-lane-gas-built binary).
**🎯 POOL GOVERNOR — SUSTAINED RECORD 8,656 tx/s; DELIVERY EXHAUSTED AS A LEVER (2026-08-10,
iter 18).** Spammer `--pool-target N` (commit d67f106): background txpool_status poller +
RateLimiter pause while pending+queued>N; POOL_TARGET env in spam-fleet-distributed.sh.
Fleet 100M governed test (RATE=2500, target 12k — the config that collapsed to 3.2k open-loop):
**8,656 tx/s, 99% full, 550ms, pool pending med 14k, queued≈0** — cliff eliminated, iter-17b
mechanism confirmed by controlled absence. RECOMMENDED: POOL_TARGET=12000 + RATE=2500 (RATE now
safe to over-provision). Pending sat above target (senders paused) → the residual gap to capacity
(8.7k@550ms vs drain 11.7k@407ms, same chain age) is CONCURRENT-INGRESS cost (admission+gossip
while consenting), NOT spammer delivery — closing it needs ingress isolation, out of scope.
Sustained ladder at 100M: 6.8k → 8.1k (RATE config) → 8.65k (governed) → 11.7k (zero-ingress).
GOTCHAS: this branch's spammer lacked --chain-id/--account-offset (lived on other branches;
target/release binary predated the branch — cherry-picked 420eb22+6d924ec back). RULE: smoke the
VERBATIM script arg string after any harness-binary rebuild (two fleet windows voided).
**⚠️ DELIVERY: OVER-OFFERING CLIFF (2026-08-09, iter 17).** Fleet 100M unpaced, backpressure spam:
RATE=1000/spammer×16 = **8.1k landed sustained, 100% full, 588ms** (fill-paced; old "5.8k plateau"
was RATE config). RATE≥1500 (24k+ offered) **collapses to 3-4k**: pool hits its cap, eviction
nonce-gaps accounts, pool fills with QUEUED, pending starves, blocks go near-empty ("txpool is
full" storm + 100ms transient backoffs throttle senders to ~300/s). `--fire-and-forget` (exists in
spammer, never used) is ALSO dead for sustained load: no nonce repair → queue collapse (2,787 tx/s,
193-tx blocks). Sustained therefore cannot reach the 11.7k capacity with open-loop spammers —
NEXT: pool-depth-governed rate (closed loop targeting pending≈2-3 blocks). FF=1 env on
spam-fleet-distributed.sh reproduces. Between same-chain load windows verify pending AND queued
drained. Full detail: MISSION-EL.md iter 17/17b.
**🎯 CLEAN DRAIN EXPERIMENT — CAPACITY NUMBERS FROZEN (2026-08-09, MISSION 4 iter 16).** Four
fresh fleet chains (60M/100M × stock/prebuild), v2 drain harness, 3 drains/point, chain age
2.7-3.6k at every drain: **60M stock 295ms/9.7k tps, 60M prebuild 272ms/10.5k (-8%, REAL),
100M stock 407ms/11.7k, 100M prebuild ~410 (nil)**. Headroom vs sustained CONFIRMED 1.6-1.8×
(delivery-paced spammers are the gap; the 1.05-2.3× uncertainty is closed). LINEAR MODEL, fits
both stock points exactly: latency(n) = EL(18ms+20.2µs/tx) + consensus(109ms+38.6µs/tx) =
**127ms + 59µs/tx → tps asymptote ~17k**; 2 blk/s crosses at n≈6,300 ≈ 132M gas; at capacity
agreeing costs 1.9× executing (39 vs 20µs/tx — the sustained 4-6×/100µs figure was
delivery-confounded, SUPERSEDED). Bigger blocks: 60→100M = +21% tps for +37% latency
(diminishing returns, user accepts the latency trade). AUTHORITATIVE chart:
`experiments/dual-el/latency-decomp-chart.py`; deck has the matching "law of the lane" frame
(pushed). Prebuild-at-100M nil because peers' growing work overlaps the hidden build.
**BRANCH `payment-el-perf` (2026-08-07, renamed from `blockstm-native-transfers` 2026-08-11 — Block-STM was never implemented here; the branch carries the native-transfer fast path, speculative prebuild, the EL measurement campaign, and the demo/bench tooling):** next experiment — Block-STM parallel execution
for native transfers on the payment lane (the "parallel execution" lever from the 1 Ggas finding:
exec 379ms/pass replayed ~5×/height is THE bottleneck; state-root 0.8ms is free). PRIOR WORK to build
on: `payment-lane-gas` commit `bd943f0` = standalone grevm-style Block-STM bench
(`experiments/utxo-state/src/bin/blockstm.rs`, 172 lines: hint DAG + level-parallel + deferred fee) +
`docs/block-stm-for-arc.md`. MEASURED there: **8.5× pooled transfers / 8.9× disjoint at realistic
per-tx cost, 1.0× hot-recipient (serial chain, expected), net-overhead when transfers are free**;
verdict "real execute-phase win but persistence>root>execute is the lane's priority order" — NOTE that
verdict predates the 1 Ggas fleet finding where exec DOMINATES (379ms vs persist 169ms vs root 0.8ms)
at full blocks, so the win case is stronger than the old verdict suggests. Cherry-picked onto this
branch as the baseline. NEXT: wire parallel execution into the real payment-EL path (grevm/reth or
Arc's executor) and re-measure fleet tps at 1 Ggas.
**🎯 BLOCK-SIZE SWEEP AT FIXED 2 blk/s — THE EL IS DONE AS A CADENCE LEVER (2026-08-08).** Re-ran the
previously-invalid sweep with the harness fixed and ALL 4 payment ELs on the best-known EL config
(`ARC_PARALLEL_TRANSFERS=1` + `ARC_EAGER_RECOVERY=1` + `--engine.state-root-fallback`), flipping the
gas limit at runtime on ONE chain (`experiments/dual-el/blocksize-sweep.sh`):

| gas | txs/blk | blk/s | ms/blk | tps | %full | exec | root | persist |
|------|---------|-------|--------|------|-------|------|------|---------|
| 25M | 1,190 | **1.99** | **504** | 2,363 | 100% | 3.8 | 10.5 | 49.0 |
| 50M | 2,380 | 1.28 | 782 | 3,043 | 100% | 9.9 | 18.6 | 448.6 |
| 100M | 4,761 | 1.50 | 665 | **7,164** | 100% | 25.7 | 27.5 | 74.9 |
| 200M | 6,805 | 1.15 | 873 | 7,795 | 71% | 30.3 | 30.0 | 86.9 |
| 500M | 9,732 | 0.65 | 1532 | 6,353 | 41% | 39.3 | 31.7 | 802.8 |
| 1G | 8,143 | 0.96 | 1043 | 7,810 | 17% | 39.0 | 33.7 | 111.8 |

- **2 blk/s IS achievable — at 25M gas: 1,190 tx/block, 504ms, 2,363 tps.** Larger blocks trade
  latency for throughput and the trade is set by CONSENSUS COORDINATION, not the EL.
- **The EL is never the constraint at any block size:** synchronous EL work (exec+root) is 14.3ms at
  25M and only 72.7ms at 1 Ggas = at most ~8% of block time (~3% at the target). Execution at the
  2 blk/s point is **3.8ms/block** after eager recovery. No cadence left to win inside the EL.
- **tps plateaus ~7-8k from 100M up** — bigger blocks stop buying throughput and only add latency
  (dominant per-height cost scales with TX COUNT: SSZ encode + streaming + voting).
- **HONEST CAVEAT: the >=200M rows are DELIVERY-bound** (blocks only 71/41/17% full — 4 local
  spammers can't offer enough load), so they measure the spammer, not the lane; only 25/50/100M
  (100% full) are chain-limited. **The 50M row is noise** (persist spiked 448ms; worse than 100M at
  half the size) — persist is the noisiest column throughout (49->803ms, non-monotonic).
- **Demo recommendation: 25M for a latency demo (true 2 blk/s), 100M for a throughput demo
  (7.2k tps @ 665ms = 3x the tps for 33% more latency).**
- Consensus: 300 consecutive blocks / 1,582,375 txs, all 4 validators identical on
  stateRoot+blockHash+receiptsRoot at every height, ALL FOUR running eager recovery (previously
  only val1) — so eager recovery is now validated as the whole-network config, not just a mixed A/B.

**⚠️ MISSION 4 ITER 15 (2026-08-09): iter-14's "1.8-2.6x headroom" DOWNGRADED TO UNCONFIRMED.**
Two problems, both mine: (1) the v1 drain harness's time attribution reduces algebraically to the
ALL-block average (diluted by fast empty blocks after exhaustion, biased LOW) — iter-14's 60M
"208ms" corrects to ~237ms from its 16-of-24-full composition; today's "93-114ms" prints were
worthless dilution. v2 harness committed (1s sampling, all-full intervals only; NOTE: the v2
rewrite initially failed to land — shell died mid-heredoc, old file kept running, caught via
output format; VERIFY FILE CONTENT). (2) **Drain height is strongly CHAIN-AGE dependent**: fresh
~237ms vs **513ms after ~1.5h churn** (measured cleanly, 38-of-39 full) — 513 is barely below the
534-559 sustained numbers. HONEST CLAIM NOW: the frontier is delivery-shaped (builder-deadline
fill-pacing mechanism PROVEN), fresh-chain capacity is meaningfully higher, magnitude 1.05-2.3x
UNCONFIRMED. Deck/paper untouched. CLEAN EXPERIMENT: fresh boot per point → blast → v2 drain x3 →
teardown; 60M/100M x stock/spec; record chain age per drain. After any killed cycle, spam nonce
state is dirty — verify pool growth (>10k/20s) before trusting a blast.

**🚨🚨 MISSION 4 ITER 14 (2026-08-09): THE FRONTIER WAS A DELIVERY CURVE — true capacity 1.8-2.6x
higher.** New `fleet/drain-test.py`: pre-fill mempool, stop ALL spam, measure the drain of
100%-full blocks = pure consensus capacity (fill time zero). Fleet, stock, unpaced: **60M drains at
208 ms = 13,705 tps** (vs "sustained" 534-559/5,300 = 2.6x); **100M at 385 ms = 11,423 tps** (vs
697/6,827 = 1.8x). MECHANISM: the builder's 500 ms deadline keeps its live best_transactions
iterator open pulling txs AS THEY ARRIVE — under our spam tooling (delivery plateau ~5.3-5.8k) the
builder waits for the pool to feed it, getPayload waits for the builder, height == txs/delivery.
**Blocks read "100% full" while being FILL-PACED — fullness was never a sufficient saturation
check; the drain test is.** REVISES: the sustained frontier + "knee at 100M" (drain 60M BEATS
100M) + the 450ms-floor/100µs-per-tx model (fit to delivery-tainted points; the "vote gap" largely
= builder fill-wait). Deck/paper numbers are now known LOWER BOUNDS — do NOT rewrite until the
drain-frontier sweep (sizes x stock/spec x n>=3, bigger backlogs, + delivered-rate sweep at fixed
size). CAVEATS: n=1/size, short drains (~48k backlog; -l resync races the drain tail — harness now
settles+verifies), drain-mode ≠ product mode but IS the capacity measure.

**🎯 MISSION 4 ITER 13 (2026-08-09): the mechanism number lands; the 60M wall is DELIVERY.**
(1) **UNPACED 40M single-box pair: stock 485ms/3,930tps vs spec 437ms/4,355tps = -10% height,
+10.8% tps** (sd 0.5%/4.8%, builds 121-153→49-69ms) — the prebuild's true floor cut, THE mechanism
number. (2) **Every 60M window for 2 days was delivery-bound**: fill time == height in ALL of them
(single-box 535-541 @ ~5.3k delivery; fleet 534-538 @ 5,311); doubling fleet spammers made it WORSE
(549ms, 96%, 4,992tps — over-offering ingress cost). RETROSPECTIVE: iter-8's single-box "-40ms" =
CPU artifact (freed build cores sped CO-LOCATED spammers; fleet spam is remote → nil). (3)
**Chain-anchored publish WORKS**: fleet hits val1 92-98% / val2 95-100% / val3 75-89% (parent-miss
collapsed); val4/wifi-alien2 chronic (39-53%) — accepted straggler. (4) Fleet 50M spec paced 524ms
HOLDS but within stock range (pacer clips both arms ≤50M). **NET: prebuild = proven -10% where
consensus-bound; at product configs it buys pacer margin until delivery >5.7k at 60M.**
**⚠️ BIGGEST OPEN QUESTION: how much of the fleet frontier is DELIVERY EQUILIBRIUM vs consensus
capacity?** At 100M the landed rate equals fill rate too. Needs: fixed size, sweep DELIVERED rate,
watch height response. Until then the frontier = upper bound on latency, lower bound on capacity.

**✅ MISSION 4 ITER 12 (2026-08-09): chain-anchored binary REGRESSION-PASSES single-box** (tailscale
needed re-auth again; fleet blocked). 28-min run, spec all 4, 60M, 100% full: **hit rates 91-93% on
all four**, parent-miss 3/25/1/0, window 530ms/5,394tps; dual-ts economics as designed (~1.5
builds/height). **Memory: dual-ts adds no visible penalty** (~245 MiB/min at 60M vs 280-415
baseline) — but the run ended in the KNOWN OOM: all pay ELs 400MiB→5.8GiB, val2 killed at the
demo's 6GiB cap at ~min 28 (third confirmed kill), which PREEMPTED the formal agreement check (one
EL down). Health evidence = 100%-full lockstep window + hit symmetry, NOT a completed 4-way check.
**RULES: single-box soaks are bounded to ~25min by the 6GiB cap; run agreement checks EARLY, not
last.** NEXT (needs tailscale re-auth): re-ship 3485bd62 → fleet spec windows → expect straggler
parent-misses to collapse.

**🔧 MISSION 4 ITER 11 (2026-08-09): chain-anchored pending publish BUILT; fleet rerun blocked on a
wedged ship.** The naive "publish on any insert" is UNSAFE (unresolvable parent ⇒ overlay silently
skips ancestors ⇒ INVALID speculative payloads). Safe fix (fork `794870f`): chain-state
`set_pending_block_chain` (nested BlockStates, exact overlay through deep unpersisted ancestry) +
engine tree assembles `[executed] + tree.blocks_by_hash(parent)` and publishes ONLY when the chain
resolves — wrong overlay impossible by construction. Built (sha `3485bd62`), image smoke-tested,
patch reverted. **Ship to remotes WEDGED (ginnythui, >20min on a 2-5min copy) and killed; remotes
still on `bfce1b06`.** Also caught+stopped a boot/ship RACE that would have produced a
mixed-version fleet measurement — **RULE: never boot while a ship is in flight; verify per-host
binary sha before any fleet run.** NEXT: re-ship → verify shas → spec 60M/75M windows → expect
val3/4 parent-misses to collapse (projected fleet 60M ≈515-520ms HOLDS) → mem-soak with spec on.
Deck: two-problem arc made explicit in the opening (state solved → coordination is problem 2).

**🎯 MISSION 4 ITER 10 (2026-08-09): DUAL-TIMESTAMP CONFIRMS THE PHASE HYPOTHESIS.** Built both ts
candidates (t0, t0+1), stash holds 2/parent, serve either. Fleet 60M same-day with FIXED
instrumentation (window counter DELTAS + remote CL build via tailscale): control 534ms/5,353 vs
spec **525ms/5,439 (sd 0.5%)**; **ts-miss 90-421 → 2-8/validator** — the class is DEAD, phase-
independent. CL build val1/2: 137-158 → **54-63ms** (93-94% hits). Residual: **miss_parent on the
stragglers** (val3 53, val4 68/window — papaduck + wifi-alien2): a CHAIN of unpublished pendings
under FCU lag defeats even the parent==pending fork condition. NEXT: under the env flag publish
pending on ANY valid insert (drop parent conditions; Arc-safe). Projected fleet 60M ≈ 515-520 =
HOLDS if val3/4 reach val1/2 hit rates. Deck restructured (levers promoted + v2.5 row, Demo 2 and
Summary refreshed, dead idea marked).

**🚨 MISSION 4 ITER 9 (2026-08-09): THE SINGLE-BOX PREBUILD WIN DOES NOT TRANSFER TO THE FLEET.**
Fork image on all 4 machines, one image both arms (env-dead edits ⇒ env-off = stock control),
distributed spam, 100% full, same-day: 60M control 534ms/5,353 vs spec **538ms/5,304 — NIL**; 75M
559/6,394 vs 546/6,539 — marginal. (Single-box same-day was 554→514.) **CAUSE: hit rate collapsed
to 47-69%** (vs 88-92%), miss_timestamp exploded (90-421/validator), miss_parent 207 on alien2
(wifi straggler). **PHASE-LOCK HYPOTHESIS:** `max(parent_ts, now)` is predicted ~0.45s early and
misses when the second boundary falls in that window — single-box 509-514ms heights are
near-resonant with the 1s grid (2 heights ≈ 1.02s) so the boundary stayed clear = the 88-92% was
partly RESONANCE LUCK; fleet 534-546ms drifts phase every height. **FIX (next): DUAL-TIMESTAMP
SPECULATION** — build t0 and t0+1, stash both, serve either; kills the class by construction on
idle cores. Rerun must also fix 2 measurement gaps: counter DELTAS per window (cumulative counters
were polluted by ~40min idle pre-load) and remote-CL block_build_time capture. SSZ decode answered
with numbers: stream+decode 18.2µs/tx + 6-11ms fixed = second-order vs vote gap 68.4µs/tx. Deck:
anatomy slide added; prebuild status honest.

**🎯 MISSION 4 ITER 8 (2026-08-09): FORK FIX LANDS — 60M HOLDS 500ms WITH PREBUILD: 514ms, 5,560
tps.** First genuinely fork-requiring change of the project (~/reth-fork commit `290b95c`, both
edits dead unless ARC_SPECULATIVE_BUILD=1): engine tree also publishes pending when the block
extends the CURRENT pending (upstream requires parent==canonical, losing the FCU race ~40% of
heights past the pacer floor); set_pending_block can anchor on the current pending state. Gate
passed against the fork with a BYTE-IDENTICAL reference hash. **Same-day result: stock 60M 554ms
misses / fork+spec 60M 514ms HOLDS, 5,560 tps, build 94-105ms, miss_parent 145→3, hit 92%.** With
prebuild the operating point moves 50M→60M at target: **4,673→5,560 tps at ≤~510ms (+19%)**,
single-machine; 150 blocks/428k txs all-4 agreement on the fork image. **DOCKER-FROM-FORK RECIPE
(landmine named):** host-built binary = GLIBC_2.38/39 crash-loop in the bookworm image; build
INSIDE rust:1.93-bookworm with workspace + ~/reth-fork mounted at IDENTICAL absolute paths
(patch resolves), CARGO_TARGET_DIR=target-docker (gitignored), overlay image FROM
arc_execution:upstream-backup + COPY, verify sha + --version smoke, then apply-fork.sh revert.
NOTE: `make build-docker` overwrites arc_execution:latest with UPSTREAM — re-run the overlay after.
OPEN: 75M (watch %full), fleet, mem-soak with spec on; flag stays OFF by default.

**🔬 MISSION 4 ITER 7 (2026-08-09): same-day stock-vs-spec at 50M+60M — fallback NOT enough above
the pacer floor.** Quiet box, 2 fresh chains, 4 windows: STOCK 50M **509ms HOLDS**/4,673tps/build
198-201 · SPEC 50M **509ms HOLDS**/4,673tps/build **90-111** (sd 0.3% — saving = pure pacer
headroom, reconfirmed same-day) · STOCK 60M 554ms/5,159 · SPEC 60M **586ms/4,874 — WORSE than
stock** (miss_parent still 145; wasted spec builds + stale stash cost ~6%). REFINED ROOT CAUSE: the
pending trigger's window at 50M was CREATED by pacer slack (natural<floor → CL pauses → FCU lands →
pending publishes). Above the floor there is NO slack; reth withholds the pending publish when
newPayload(N) races FCU(N-1) (~40% of heights) and no Arc-crate trigger can see the block in time.
**Flag must stay OFF for >50M until fixed. The fix is finally the FORK's job:** one-line relaxation
in reth engine-tree insert_block_or_payload (publish pending even when parent not yet canonical —
sound on Arc, no side-chains; gate behind ARC_SPECULATIVE_BUILD env), then docker-from-fork. Also:
tonight's box regime faster across the board (stock 50M 509 vs 537-539 before) — same-day pairs only.

**🔬 MISSION 4 ITER 6 (2026-08-09): 60M exposed a TRIGGER GAP in speculation v1 — fixed, not yet
re-measured.** Quiet box (pre-flight load check). 50M spec healthy (534ms, build 89-103ms); at 60M
build jumped to 148-235ms with a NEW miss class: **miss_parent ~46% on val3** (0 at 50M). ROOT
CAUSE: reth sets the pending block ONLY if the newPayload'd block's parent is already canonical;
past ~550ms heights, newPayload(N) races FCU(N-1), reth skips the pending update, the watcher goes
blind, stash goes stale → guaranteed miss_parent. v1 worked at 50M by timing luck. FIX: canonical-
head fallback trigger in the watcher (~15 lines, same crate); gates pass; binary typechecks. The
60M row measures v1's failure mode, NOT the prebuild's potential. Next: re-measure 50/60/75 with
fallback + same-day stock controls.

**⚠️ MISSION 4 ITER 5 (2026-08-09): NO VALID DATA — box contended (load 38/16 cores, daytime use);
50M all-spec read 594-611ms sd 12-17% vs 527ms sd 2.3% yesterday on identical config. ALL numbers
discarded. The gas-at-500ms-with-prebuild sweep needs a quiet box or the fleet.**

**🎯 MISSION 4 STEP 3: ALL-4 CADENCE A/B (2026-08-09) — build halved network-wide; the 500ms PACER
absorbs the saving.** Two fresh chains, identical 50M config/load, 12-min windows: stock
539ms/4,419tps/build 186-218ms vs all-spec **527ms/4,513tps/build 81-115ms, hit 88-90%**, both arms
100-block agreement. Cadence +2% only (not +15-20%) **because the demo paces to BLOCK_TIME_MS=500**
— the saving becomes PACER HEADROOM (39ms over floor -> 27ms over), the product's stable-latency
behaviour. **Next real question: how much BIGGER a block holds 500ms WITH prebuild** (mission 3:
25M holds, 50M@537 misses). Unpaced probe on the aged chain (553ms) = INVALID (state-grown chain +
sd 7.9%); needs fresh same-age unpaced pair. OPS: spawn spammers via setsid+detached script — nohup
children die with a timed-out harness shell.

**🎯 MISSION 4 STEP 2: SPECULATIVE PREBUILD LIVE-VALIDATED (2026-08-09) — proposer build HALVED,
85.5% hit rate.** Single-machine demo, 50M, ~5.4k tx/s offered, flag on val1's payment EL ONLY vs 3
stock. Mechanism confirmed from CL source first: `generate_block` calls get_payload IMMEDIATELY
after FCU (no fixed wait) + `wait_for_payload` → getPayload latency ≈ build time → a hit removes it
entirely. **CL `block_build_time` (both lanes, CL wall clock — NOT an EL sub-metric): val1 83.8 ms
vs stock 163-169 ms.** Hit rate 148/(148+24+1)=**85.5%** (all misses = second-rollover
miss_timestamp; zero miss_parent/miss_other — learned attrs predict perfectly; 685 spec builds =
one per height). **Correctness: 200 blocks / 476k txs, all 4 identical on
stateRoot+blockHash+receiptsRoot+logsBloom**, zero stalls. **Memory: no stash cost** (1.951 vs
1.962 GiB). NOT yet claimed: cadence (1-of-4 ≈ 18 ms expected = invisible in ±15% noise) — next:
all-4 OFF vs all-4 ON cadence A/B (legitimate now correctness is proven vs stock), then the fleet.
Deploy: `PAY_EL_ENV='-e ARC_SPECULATIVE_BUILD=1'` (or PAY_EL<i>_ENV per validator).

**✅ MISSION 4 STEP 1: SPECULATIVE PREBUILD IMPLEMENTED — NO reth fork needed (2026-08-09).**
`ARC_SPECULATIVE_BUILD=1` (default OFF), entirely in `arc-execution-payload` (`src/speculative.rs`
+ 2 hooks in `payload.rs`). Zero fork changes because (1) reth 2.3's engine tree ALREADY
`set_pending_block`s a newPayload'd block whose parent is the canonical head (Arc's case every
height) and `state_by_block_hash` resolves the pending slot; (2) a 25 ms poll of `pending_block()`
is the trigger (no public subscription; <=5% of the 350-900 ms vote gap). Prediction: timestamp =
`max(parent_ts, now_secs)` (CL's own formula, lanes lockstep -> misses only on a second rollover,
expect ~50-70% hits); fee_recipient/prev_randao LEARNED from the last real request; pbbr = N.hash.
**Trap found: the pool must be pre-filtered with N's tx hashes** — stale nonces + `mark_invalid`'s
remove-descendants semantics would empty every sender chain (near-empty speculative blocks).
Serving: `try_build` -> Freeze iff ALL attrs match; labelled miss counters
(`arc_speculative_build_outcome_total{outcome=hit|miss_timestamp|miss_parent|miss_other|miss_empty|built}`);
fail-safe (wrong prediction = idle CPU, never a wrong block). Gate leg 3: stash==fresh byte
equality + timestamp-miss falls through; all gates pass under both flag settings, reference hashes
unchanged, full binary typechecks. **NOT live-validated yet** — next: build-docker, val1-only A/B
vs 3 stock peers, 100+ block agreement, HIT RATE, height movement, mem-soak alongside.

**📌 CLAUDE.md CATCH-UP (2026-08-09): iterations 9-15 + mission 4 step 0 — six updates silently
failed to land here** (an anchored str.replace no-op'd after iter 8 wrote a ##-heading; every later
update anchored on a **-heading that never existed. RULE: `assert anchor in s` before any anchored
doc edit). Full detail for all of these is in experiments/reth-fork/MISSION-EL.md; the essentials:
- **MEMORY (iters 9-12):** payment EL grows ~280-415 MiB/min under sustained load (~0.65-0.93 KB/tx),
  ALL nodes, no plateau within 35 min; alien2's 11 GiB cap OOMs (predicted, then confirmed,
  `oom=true exit=137`). NO config knob moves it (persistence thresholds, cross-block-cache 16x
  smaller, state cache DISABLED: all ±1% once normalised against the untreated machine-pair ratio —
  and the earlier "persistence = 16% slower" claim was RETRACTED as a machine artifact; ginny runs
  15% slower than ginnythui untreated). **NOT A LEAK:** stops load → memory releases at 130-163
  MiB/min (~40-50% of growth rate). Operational rule: size for the longest CONTINUOUS burst
  (~0.35 GB/saturated-minute above idle); a dying node silently corrupts throughput measurements
  (dying fleet → blocks fill → reads as capacity), so run fleet/mem-soak.sh alongside any sustained
  run.
- **CONSENSUS SPLIT (iter 14, `fleet/consensus-split.sh` + chart):** height ≈ **~450 ms fixed
  consensus floor + ~100 µs/tx above ~2,400 tx**. EL busy 8-15% at every size (25M: 92% consensus).
  25M→50M doubles txs for +12.9 µs/tx (still on the floor); above that, agreeing costs 4-6x
  executing (50→100M: cons 93.2 vs exec 21.5 µs/tx). State root FALLS per tx as blocks grow
  (3.19→0.82 µs/tx); only consensus degrades with scale.
- **SPECULATIVE PREBUILD CASE (iter 15):** proposer build = 97.9 ms at full 50M blocks = 57.6%
  tx_execution + 40.2% post_execution (<1% rest) — real pre-computable compute on the critical path
  while the EL idles 350-900 ms in the vote gap. The three existing builder flags
  (share-execution-cache / share-sparse-trie / suppress-persistence-during-build) are ALL NULL
  (±2% vs in-run control, `fleet/build-time.sh`). RoundRobin is deterministic (selects on height +
  ROUND), so the EL can know/infer its turn; ~20% cadence at every size; 50M: 551→~454 ms = under
  the 500 ms target ≈ doubles tps at 2 blk/s. Arc credits the beneficiary EVERY tx → speculative
  state is bound to one proposer identity. MISSION 4 = implement this in the reth fork.
- **MISSION 4 STEP 0 DONE (build gate):** `crates/execution-payload/examples/build_gate.rs` drives
  the REAL `arc_ethereum_payload` offline (MDBX + init_genesis = real trie roots; txs injected via
  the `best_txs` closure seam — `_pool` is unused). Checks: in-process determinism, stock vs
  ARC_PARALLEL_TRANSFERS=1 byte-identical (first offline coverage of the fast path on the BUILDER),
  gas arithmetic. Run it (both flag settings, diff MUST be identical AND NON-EMPTY — an early run
  passed vacuously on two empty outputs when a panic went to suppressed stderr) before ANY builder
  change. Known benign quirk: localdev chainspec genesis header declares root 0xbc32... but the
  computed alloc root is 0x0c6b... (builder derives from DB; self-consistent).

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

**📊 DECK ALIGNED (2026-08-09):** the deck had been headlining 1 Ggas at "9.5k/15k tx/s" — the source
of the "bigger blocks bought throughput" impression. Slide now compares 100M vs 1 Ggas directly
(7,398 tx/s @ 644ms vs 7,272 @ 5,478ms) and carries the marginal-cost table. Corrected my own sloppy
bullet claiming state root was "20-50ms at 9.5k tx/block" (mixed sync/async measurements; fleet
reality is 2.3ms @ 4,761 tx and 0.3ms @ 39,832, not growing with block size). Non-reproduction of the
9.5k/15k figure is stated on the slide.

**🔬 WHERE A BIGGER BLOCK'S ms GO — MEASURED; MISSION 3 CLOSED (2026-08-09).** Used Arc's own
`reth_arc_payload_total_duration_seconds` (proposer build) + beacon-engine metrics. Both points 100%
full, 4 machines, distributed spam:
| gas | txs/blk | height | build | newPayload | exec | root | voteGap | remainder | tps |
|------|---------|--------|-------|-----------|------|------|---------|-----------|-----|
| 50M | 2,380 | 515ms | 68.1 | 43.5 | 35.9 | 3.0 | 347.6 | 124.2 | 4,619 |
| 200M | 9,523 | 1,216ms | 176.7 | 124.8 | 117.2 | 1.7 | 836.2 | 254.5 | 7,834 |
- **50M REPRODUCES: 1.94 blk/s / 515ms / 4,619 tps** vs 1.92/522/4,563 on a different chain (1.5%).
  Headline stands — this check mattered, an earlier single-run 50M claim had to be requalified.
- **MARGINAL COST OF A TX = ~98us OF HEIGHT; only ~11us is our execution.** Split of the 50M→200M
  growth: proposer build +108.6ms (15.2us/tx, 15.5%) · own newPayload +81.3ms (11.4us/tx, 11.6%) ·
  **voteGap +488.6ms (68.4us/tx, 69.7%)** · remainder/stream+decode +130.3ms (18.2us/tx, 18.6%).
- **~70% lands in the VOTE GAP** = newPayload-done → next FCU. NOT idle network: it contains the
  other validators receiving/decoding/EXECUTING the same block + 2 vote rounds. Each tx is executed
  **~5x across the network** (1 build + 4 validates), all on the round's critical path, quorum gated
  by the SLOWEST. State root is 1.7-3.0ms and does NOT grow with block size (1.7ms @ 9,523 tx vs
  3.0ms @ 2,380) — it cannot be the answer.
- **MISSION 3 CLOSED. No further tps at 2 blk/s without touching consensus.** 88% of a tx's marginal
  cost is outside our execution. CL-side levers (out of scope): ship tx HASHES not full txs (peers
  already have them; the proposal duplicates ~1.2MB at 100M), pipeline exec of N against consensus of
  N+1, homogenise validators so quorum isn't gated by the slowest box.

**✅ MISSION 3 GOAL MET ON REAL HARDWARE (2026-08-09): 50M HOLDS 2 blk/s AT 4,563 TPS.** Measured the
fleet's LOW-LATENCY end (all prior fleet points started at 200M). 4 machines, distributed spam, stock:
| gas | spm | txs/blk | %full | blk/s | latency | tps |
|------|-----|---------|-------|-------|---------|-----|
| 25M | 16 | 1,190 | 100% | 1.89 | 529ms | 2,249 |
| **50M** | 16 | **2,380** | **100%** | **1.92** | **522ms** | **4,563** |
| 100M | 16 | 2,845 | 60% | 1.81 | 553ms | 5,144 |
| **100M** | 32 | **4,761** | **100%** | 1.55 | **644ms** | **7,398** |
- **GOAL MET: 50M (2x the 25M baseline) holds 2 blk/s** — 1.92 blk/s, 522ms, 100% full, **4,563 tps
  = 1.93x the 2,363 baseline**, all 4 validators agreeing.
- **BEST POINT: 100M saturated = 7,398 tps @ 644ms** — beats every larger block on BOTH axes (200M
  7,150@864ms; 1Ggas 7,272@5,478ms). **100M→1Ggas = 10x block, ZERO tps gain, 8.5x latency.**
  The frontier has a KNEE at ~100M; past it is pure latency cost. Rule: "smallest block that reaches
  the plateau", NOT "biggest block that fits".
- The two 100M rows re-show the over-offering tradeoff, and here it PAYS: 16→32 spammers costs +16%
  latency for +44% tps. At 200M the same doubling bought +6% tps for +45% latency. Favourable at the
  knee, unfavourable past it — which is another way to locate the knee.

**🎯 FLEET FRONTIER — RECONCILES "10k tps" vs the 4.7k single-box number (2026-08-09).** 4-machine
fleet, stock config, distributed spam (one set per machine on its LOCAL pay EL):
| gas | spammers | txs/blk | %full | blk/s | latency | tps |
|------|----------|---------|-------|-------|---------|-----|
| 200M | 16 | 6,175 | 65% | **1.16** | **864ms** | **7,150** |
| 200M | 32 | 9,523 | 100% | 0.80 | 1,250ms | 7,616 |
| 500M | 16 | 15,430 | 65% | 0.53 | 1,876ms | **8,226** |
| 1G | 16 | 39,832 | 84% | 0.18 | 5,478ms | 7,272 |
- **THROUGHPUT SATURATES ~7-8k tx/s ON THE FLEET; ONLY LATENCY CHANGES.** 200M→1Ggas (5x block) moves
  tps 7,150→7,272 (nil) but latency 864→5,478ms (**6.3x worse**). **Operate at the SMALLEST block that
  reaches the plateau: 200M @ ~864ms.** Bigger blocks buy nothing on real hardware.
- **The 9.5k/4.7k gap = hardware + block size, not regression.** Single box = 4 validators (12
  containers) on 16 shared cores, each re-executing every block; fleet = 1 validator per machine. Same
  200M: fleet 7,150 tps @ 864ms vs single box 6,785 @ 1,404ms — same plateau, **1.6x better latency**.
- **OVER-OFFERING LOAD IS COUNTERPRODUCTIVE (quantified):** 16→32 spammers at 200M filled blocks
  65%→100% for **+6% tps but +45% latency**. **"100% full" is the WRONG success criterion for a
  latency-sensitive lane** — the ingress cost of surplus txs exceeds what fullness returns.
- **THESIS STRENGTHENS AT SCALE: state root 0.3ms on a 39,832-tx block** (1.0ms at 15,430). Commitment
  stays negligible as blocks grow 5x; execution is what scales (61.8→479.3ms).
- Today's 1 Ggas 7,272 tps vs the recorded 9.5k avg/15k peak: different chain age/machine state, and
  15k was a PEAK vs a 120s average here. Both on the same 7-9k plateau.

**🎯 2-D SWEEP (BLOCK TIME x GAS): BLOCK TIME IS NOT A THROUGHPUT KNOB (2026-08-09).** First sweep
with EVERY point 100% full — distributed load (2 local + 8 ginnythui + 5 alien2 spammers over
tailscale against this box's pay EL); local-only load saturates ~6k tx/s and can't fill 100M+ blocks.
Harness `experiments/dual-el/blocktime-sweep.sh`, chart `blocktime-chart.py`.
| target | 50M | 100M | 200M |
|--------|-----|------|------|
| 250ms | 1.61 blk/s, 3,840 tps | 1.10, 5,234 | 0.71, **6,785** |
| 500ms | 1.47, 3,500 | 1.06, 5,025 | 0.68, 6,522 |
| 1000ms | **1.00 HELD**, 2,378 | **1.00 HELD, 4,748** | 0.67, 6,382 |
- **`targetBlockTimeMs` is a CEILING, never a floor.** Same gas across targets: 50M → 1.61/1.47/1.00
  blk/s at 250/500/1000ms. A shorter target changes nothing (chain already slower); a longer one
  THROTTLES (50M paced from ~1.5 down to exactly 1.00, costing ~1,100 tps). It is a latency-
  predictability knob, not a performance one.
- **The GAS LIMIT is the frontier, with steep diminishing returns:** natural cadence 50M ~1.54 blk/s
  /~3,670 tps · 100M ~1.08/~5,130 · 200M ~0.70/~6,650. **4x gas = 1.8x tps, 2.2x latency.**
- **Offered load beyond what FILLS a block still costs cadence:** 50M did 1.96 blk/s with 6 local
  spammers vs 1.47-1.61 with 15 distributed ones at IDENTICAL 100%-full composition — pure ingress
  cost (RPC/mempool admission + gossip for txs that won't fit). This is why the earlier "40M holds
  2 blk/s" is a LIGHT-LOAD number.
- **BEST ANSWERS: "1 blk/s" → 4,748 tps @ 100M, held at exactly 1.00 blk/s** (the only config that
  both saturates AND holds its target, ~8% headroom is why). **"2 blk/s" → NOT reachable at any gas
  under saturating load**; fastest saturated point is 1.61 blk/s @ 50M. Peak tps 6,785 @ 200M but
  1.4s blocks and holds no target.

**⚠️ REQUALIFIED (2026-08-09 iter 2): 50M is MARGINAL, not the ceiling; 40M is the reproducible
2 blk/s point.** Reverse-order sweep on a fresh chain (60→55→50, biggest block gets freshest chain)
gave 50M **1.72 blk/s / 582ms** vs the forward sweep's **1.96 / 511ms** — same size/load/config, ~12%
apart. So chain age was NOT the explanation (reverse order ruled it out); it is plain run-to-run
variance (±15% on this box). **Quote 40M: 1.98 blk/s, 504ms, 3,777 tps, reproducible.** 50M = "marginal,
1.7-2.0 blk/s". The iteration-1 headline was over-fitted to one run.
**THE 1 blk/s QUESTION IS UNANSWERABLE ON THIS BOX — the SPAMMER is the ceiling, not the chain.**
100/200/300/400M with 12 spammers: 67%/40%/38%/19% full, delivered tps stuck at 5.5-6.2k. Doubling
spammers 6→12 barely moved delivery (local generation saturates ~6k tx/s; they compete with 12
containers for 16 cores — reproduces the mission-1 "more spammers made it worse" finding). Those
cadences (300M @ 1.15 blk/s) come from PARTIAL blocks and are NOT capacity numbers. Needs
fleet/spam-fleet-distributed.sh; attempted but `tailscale ssh` requires interactive re-auth
(ginnythui + alien2 online, papaduck absent from the tailnet).
**BEST SATURATED tps TO DATE: 7,164 @ 100M / 665ms / 1.50 blk/s, 100% full** (earlier 4-spammer
sweep) — higher than today's 12-spammer run at the same size.

**🎯 MISSION 3: 50M GAS HOLDS 2 blk/s AT 4,660 TPS = 1.97x THE BASELINE (2026-08-09).** The win came
from FIXING THE MEASUREMENT, not from optimising. Fine-grained sweep (one chain, runtime gas flips,
all 4 pay ELs on `--engine.state-root-fallback`, 75s windows, **6 spammers so every point is 100%
full**): 25M 2,363tps@504ms · 30M 2,835@504 · 40M 3,777@504 · **50M 4,660tps@511ms = the ceiling** ·
55M 4,427@591 · 60M 4,986@573 · 75M 5,186@689. The earlier "only 25M holds" answer was an artefact of
UNDER-DELIVERY (4 spammers): 50M previously read 782ms/1.28blk/s with persist "spiking" to 448.6ms;
re-measured it is 511ms/1.96blk/s with persist 70.4ms. Chart: `experiments/dual-el/blocksize-chart.py`.
CONFOUND: sizes sweep sequentially on a GROWING chain, so later points carry more state (55M ran last
and lost to 50M on BOTH axes — partly chain age). Above 50M tps flattens (4,660→4,986→5,186) while
latency climbs, so 50M is near-optimal on both axes anyway.
- **❌ LEAD #1 PERSISTENCE — CLOSED, MAKES IT WORSE.** Same sweep with `--engine.persistence-threshold
  64 --engine.memory-block-buffer-target 128` on all 4: 30M 1.93 / 40M 1.65 / 50M 1.36 / 60M 1.13 blk/s
  (vs 1.99/1.98/1.96/1.75). Mechanism visible in the columns: **state root 2-3x more expensive**
  (12.5/19.3/27.8/31.7 → 43.6/63.8/61.0/69.6ms) while persist did NOT drop — holding 64-128 blocks in
  memory makes root walk a deeper in-memory overlay. Persistence was never the limiter: fsync here is
  1.28ms/op, persist is async, and it never correlated with cadence.
- **🚨 FOURTH CONSENSUS BUG (zero-value logs), found by the newly-extended gate.** Adding EIP-2930
  (empty access list) + zero-value transfers to `Workload::Mixed` immediately caught it: state digests
  matched but receipts did not — stock 6,538 logs vs fast path 7,000, the 462 gap being exactly the
  zero-value txs. `before_frame_init` only logs via `Some((from,to,amount)) if !amount.is_zero()`, so a
  ZERO-VALUE transfer emits NOTHING; the fast path emitted one. Fixed (no logs when `value.is_zero()`).
  2930 needed no fix — `effective_gas_price` already covered it. Offline-validated only (the spammer
  never sends zero-value txs, so live wouldn't exercise it); flag stays OFF by default.
  **FOUR bugs, ONE shape: every one came from ASSUMING what a tx is instead of asking. Three of four
  were invisible to state comparison alone.**

**🚨 SECOND CONSENSUS BUG IN THE FAST PATH — LEGACY-TX FEES (2026-08-08).** Same blind-spot class
as the log bug: both gates only ran PURE EIP-1559 TRANSFER blocks. Live mixed load
(`--mix transfer=60,erc20=25,guzzler=10,legacy=5`), val1 fast path vs 3 stock → val1 diverged at
block 50 on the **STATE ROOT** (receipts matched!) and stalled at 49 while the network reached 125.
- **CAUSE:** fast path hardcoded 1559 shape `min(max_fee, basefee + max_priority.unwrap_or(0))`. A
  LEGACY (type-0) transfer with no calldata IS fast-path eligible, has no priority field → formula
  collapsed to `min(gas_price, basefee)` = **basefee**, but legacy's effective price is `gas_price`
  outright. Sender under-charged, beneficiary (Arc credits the FULL fee) under-credited. Gas still
  21k, log unchanged → **receipts identical, only the state root moved** — invisible to the
  receipts digest added hours earlier.
- **FIX:** `tx.effective_gas_price(Some(basefee))` — ask the tx, don't assume a shape. Correct for
  legacy/2930/1559 by construction.
- **GATE EXTENDED (`Workload::Mixed`):** 7,000 txs, every 7th with calldata (forces general-EVM
  path → overlay flush + cache drop), every 5th of the rest LEGACY; prints a post-state digest for
  cross-gate comparison. Reproduced the bug instantly (0xefd6… vs 0x1769…), matches after the fix.
  NOTE: the calldata/invalidation half alone reproduced NOTHING — overlay/cache invalidation is
  fine; it was purely the fee shape.
- **VALIDATED:** 200 consecutive blocks, 497,531 txs, confirmed-mixed composition (19,038 type-2,
  1,032 type-0, 6,885 with calldata), all 4 identical on stateRoot+blockHash+receiptsRoot+logsBloom,
  zero invalid blocks.
- **PATTERN — 3 bugs, ONE shape: every fast-path bug came from ASSUMING what a tx is instead of
  asking it** (assumed no logs; assumed 1559 fees). Still-uncovered fast-path-ELIGIBLE shapes:
  EIP-2930 with empty access list, and zero-value transfers. Add these before ever considering
  default-on.

**🚨 CONSENSUS BUG FOUND + FIXED IN THE NATIVE-TRANSFER FAST PATH — AND ITS "1.58x" WAS MOSTLY THE
BUG (2026-08-08).** `ARC_PARALLEL_TRANSFERS=1` was FORKING THE CHAIN against unmodified nodes. Arc
emits a log for EVERY native value transfer (`ArcEvm::before_frame_init` → `crate::log`); the fast
path bypassed the interpreter and emitted NONE, so receipts + logsBloom differed from stock while
post-state was byte-identical. Caught by the first-ever A/B against UNMODIFIED peers (val1 fast
path, val2-4 stock): val1 rejected the first non-empty block with `receipt root mismatch`.
- **Why 6 prior "validations" missed it:** (1) the offline gate compared post-STATE only — receipts
  carry type/status/cumulative-gas/**logs**, all of which can differ with state intact; (2) every
  live run set the fast path on ALL 4 validators via the global `PAY_EL_ENV`, so four nodes running
  identical modified code agreed with each other while all diverging from stock. **RULE: A/B an
  optimisation against UNMODIFIED peers, never against copies of itself.** It also silently dropped
  the Transfer events wallets/indexers consume.
- **FIX (executor.rs):** emit the same log the interpreter would — EIP-7708 `Transfer` from Zero5
  on (self-transfers suppressed), legacy `NativeCoinTransferred` before — via `crate::log` + the
  same `is_arc_fork_active` gate, so it is correct by construction.
- **GATE STRENGTHENED:** `parallel_transfer_bench` now prints a receipts digest + log count; both
  gates must MATCH. Pre-fix: stock 0x93a9…/0x9c0b… with 47,618 logs vs fast path 0x21e2… with 0
  logs (identical digest across both workloads was the tell). Post-fix: exact match.
- **HONEST PERF — ~3%, not 1.58x.** Correctly emitting logs, simultaneous same-block A/B: exec
  87.1→79.5ms (1.10x), per-tx 7.6→3.77us (2.0x), but **newPayload 118.3→115.0ms = 1.03x** — ~0.5%
  of a height. Log construction is a big share of what revm does for a transfer, so the old
  1.39-1.58x was largely the omitted work. **KEEP IT GATED OFF; not worth deploying for 0.5%.**
- **VALIDATED POST-FIX:** 200 consecutive blocks / 952,200 txs, MIXED config (val1 fast path vs 3
  stock), all 4 identical on stateRoot+blockHash+receiptsRoot+**logsBloom**, zero invalid blocks.
- Retroactively invalidates the mission-1 "✅ MISSION COMPLETE — 1.58x / 120 heights zero
  divergence": that run proved self-consistency across 4 identically-modified nodes, not correctness.

**❌ CORRECTION: THE "EAGER RECOVERY 4x" BELOW WAS A MEASUREMENT ARTIFACT — REVERTED (2026-08-08).**
It was measured with `transaction_wait` + `transaction_execution`, which do NOT sum to the cost of
newPayload; eager recovery moved work OUT of the execution loop (into iterator construction) where
those histograms cannot see it. Checked against the independent
`reth_consensus_engine_beacon_new_payload_latency`, same box, same 4,761-tx blocks, SIMULTANEOUS
window, eager on val1 only: exec 58.2->19.9ms and wait/tx 8.7->0.9us, BUT `other` inside newPayload
4.1->43.2ms, and **TOTAL newPayload 85.9ms (eager) vs 81.1-87.5ms (stock) = unchanged within
noise.** WHY: reth OVERLAPS recovery with execution, so `wait` is pipeline overlap, not waste;
recovering eagerly makes it a serial barrier and gives back exactly what it saves. Reverted in full
(evm.rs back to plain delegation, rayon dep dropped); kept `recovery_bench.rs`, `recovery-probe.sh`,
the `PAY_EL<i>_ENV` per-validator hook, and this record. Post-revert both gates IDENTICAL + 200
blocks / 952,200 txs with all 4 agreeing. **RULE: an EL optimisation only counts if
`reth_consensus_engine_beacon_new_payload_latency` moves — sub-metrics can be relocated.**

**❌ HYPOTHESIS #1 (engine-API ingestion) REFUTED — and it was the top-ranked open suspect
(2026-08-08).** New `experiments/dual-el/height-decomp.sh` splits a height via reth's beacon-engine
metrics. At 100M gas / 4,761 txs / 715ms height, stock: newPayload 113.3ms (15.8%) = exec 78.2 +
root 29.3 + **other 5.8**; newPayload->FCU 342.5ms (47.9%, CL voting, EL IDLE); FCU 1.0ms;
remainder 258.6ms (36.2%, next-proposer build + SSZ + streaming, EL IDLE). **EL busy 16%, idle
84%.** The only place a hidden JSON-decode/ingestion cost could hide is that 5.8ms (~0.8% of the
height) — so **IPC for the payment lane cannot move cadence** and that step is closed (CL<->EL is
localhost in both topologies anyway). Independently reconfirms the 10/90 EL/non-EL split from a
different metric family.

**🎯 EAGER PARALLEL SENDER RECOVERY — payment-lane execution phase 4x faster, arc-evm only, NO reth
fork (2026-08-08). ⚠️ SUPERSEDED — SEE THE CORRECTION ABOVE; THIS WAS REVERTED.** The `newPayload` execution loop was spending ~78% of its time NOT executing:
blocked in `transactions.next()` waiting on sender recovery. Three measurements corrected three
wrong assumptions before any code was written.
- **It is not the line we thought.** `payload_validator.rs:323` (`try_into_recovered`) is the
  `BlockOrPayload::Block` branch. newPayload takes the other one:
  `EthEvmConfig::tx_iterator_for_payload`. `ArcEvmConfig` only DELEGATED to it → Arc can override
  it in arc-evm. **The reth fork was never needed for this.**
- **It is not CPU contention.** New `experiments/dual-el/recovery-probe.sh` diffs reth's
  `transaction_execution`/`transaction_wait` histograms per validator: wait/tx was 10.05 vs 10.86us
  at 2 vs 8 competing spammers, ~10us under both state-root strategies, while our executor's own
  time moved 2.87→4.14us. Structural.
- **It is not the cryptography — it is reth's ordered per-tx delivery.** New
  `crates/evm/examples/recovery_bench.rs`: decode 0.22us/tx, ECDSA recover **33.42us/tx serial but
  4.02us/tx on 16 threads (8.5x)**. The live loop realised only ~3.3x, because
  `spawn_tx_iterator` streams recovery through `for_each_ordered_in` one tx at a time.
- **FIX (~40 lines, `ARC_EAGER_RECOVERY=1`):** override `tx_iterator_for_payload` to recover the
  whole payload up front on rayon and hand reth a precomputed vector. Two-state `PayloadTx::{Done,
  Raw}`, both through one `recover_payload_tx` → a bad tx surfaces at the same index; flag-off stays
  lazy as upstream drives it. `EAGER_RECOVERY_MIN_TXS = 30` mirrors upstream's small-block
  threshold so an idle 2 blk/s lane is untouched. Also added `PAY_EL<i>_ENV` to
  launch-payment-els.sh (per-validator docker `-e`) — that is what makes a same-box A/B possible.
- **MEASURED (same box, same blocks, 4-way A/B):** val1 eager+fallback **3.90us/tx loop, 0.85 wait,
  18.9ms/blk** vs val2 fallback-only control 15.36/11.99/74.6 vs val3-4 stock ~21/16/101-105.
  **3.9x vs same-config control, ~5.4x vs stock, wait/tx −93%** (at ~10.3k txs/blk: 36.9 vs
  175.7ms). Execution is no longer recovery-dominated — wait fell 78%→22% and our executor is now
  the majority of what is left.
- **VALIDATED:** 350 consecutive blocks / 1,567,888 txs, all 4 validators identical on
  stateRoot+blockHash+receiptsRoot at EVERY height, val1 eager against 3 non-eager peers; 34 blocks
  below the threshold so both branches ran. Both offline gates IDENTICAL.
- **HONEST LIMIT:** cadence did NOT move (0.76–1.5 blk/s). Like every EL win here, it lands in
  per-block exec_ms, not fleet tps — ~2.4s/height is consensus coordination, which the mission's
  hard constraint puts out of scope. Not yet measured on the 4-machine fleet; still gated off by
  default.

**MEASURED (2026-08-07, commit 55d3a60): the EVM/executor layer is NOT the bottleneck.**
`crates/evm/examples/transfer_bench.rs` runs the REAL ArcBlockExecutor on a full 1-Ggas block
(47,618×21k transfers, 16k closed accounts, bundle tracking on): **102.5ms = 2.15 µs/tx** (465k tx/s
serial) vs the live node's ~11 µs/tx (379ms/34k block) → **~80% of live exec cost is AROUND the
executor**: reth's engine-tree per-tx loop (payload_validator.rs:1298 drives per-tx, streams receipts
to the root task, coordinates prewarm) + state-provider/cache lookups. IMPLICATION: fast-pathing Arc's
executor crates can't fix it; the modified reth must attack the engine-tree loop/provider layer —
i.e. a vendored reth fork with a batched/parallel transfer path in the payload validator (Arc handler
detail: base fee NOT burned, beneficiary credited EVERY tx → parallel scheme must defer coinbase).
Arc's per-transfer customizations (for any fast/parallel path): blocklist SLOAD check on
sender+recipient (unmetered) + NATIVE_COIN_CONTROL load in pre_execution; full fee (base+tip) to
beneficiary in reward_beneficiary. Engine hot path: newPayload → payload_validator per-tx loop —
`execute_block` override would NOT be hit (only BasicBlockExecutor uses it). GOTCHA: arc-evm
cfg(test) has 91 pre-existing compile errors (revm-40 bump never fixed tests) — benches must be
example targets. perf is locked on this box (perf_event_paranoid=4, no sudo).
**🎯 PARALLEL TRANSFER EXECUTION — ALGORITHM DONE, DIFFERENTIAL-VERIFIED (2026-08-08, commit e4e8dc4).**
`crates/evm/examples/parallel_transfer_bench.rs` runs a full 1-Ggas block (47,618 transfers) BOTH ways
through the REAL ArcBlockExecutor and compares post-state per account: **pool workload 105.4→25.7ms
(4.11x), closed/ring 93.6→22.0ms (4.25x), state IDENTICAL both**. ALGORITHM (grevm-style lazy balance
updates, NOT textbook Block-STM): partition by SENDER (nonce/balance = real RMW, one owner, in-order);
merge every other touched account (recipients + the beneficiary Arc credits EVERY tx) as a COMMUTATIVE
BALANCE DELTA aggregated per worker. No aborts/retries, deterministic — inverts the hard case, since
optimistic Block-STM collapses to 1.0x on this hot-recipient workload (`0x1000+nonce` recipients; see
spammer `transfer_recipient`). Perf notes: per-worker delta aggregation took it 2.2x→4.25x (the serial
merge of ~140k per-tx entries was the bottleneck); chunk = partitions/(threads*4) for load balance.
GOTCHA the differential test caught immediately: `State` keeps changes as transitions —
`merge_transitions(BundleRetention::PlainState)` is required before `take_bundle()` or the bundle is empty.
**WIRING BLOCKER FOUND + SOLVED ON PAPER (2026-08-08, commit b307b0d) — NOT YET IMPLEMENTED.** Parallel
exec must bypass reth's per-tx loop, but that loop builds RECEIPTS whose type is `E::Result`
(executor-specific) — generic reth code CANNOT construct it, so batch execution must live in arc-evm and
be called through a trait that does not exist yet. Closed alternatives (all checked): buffering inside
`execute_transaction` breaks the BUILDER; parallel-prewarm+serial keeps the 102ms serial floor; blanket
`impl<E: BlockExecutor>` blocks the real impl (no specialization). SETTLED: add a small `BatchExecute`
trait to the FORK's reth-evm (`set_batch_mode` / `flush_batch`, default no-ops), bound it in
`payload_validator::execute_transactions`, one-line empty impl for reth's EthBlockExecutor, real impl on
ArcBlockExecutor (buffer -> gate on all-plain-transfers -> snapshot touched accts + blocklist slots (avoids
any Sync bound on the provider) -> parallel per verified algorithm -> patch each ResultAndState balance to
`current_real + (post-pre)` and feed the EXISTING commit_transaction so receipts/gas/bloom stay production
code). Worker EVM cfg MUST match production (evm.rs:2050) or state diverges. Step-by-step in
experiments/reth-fork/README.md.
**Also: do NOT buffer txs inside ArcBlockExecutor unconditionally** — the BUILDER routes
through `execute_transaction_with_commit_condition` and inspects each tx result to decide inclusion, so
buffering breaks block production. Instead parallelize VALIDATION only (building stays serial): add
`ArcBlockExecutor::execute_transactions_parallel(txs)` and call it from the fork's
`payload_validator.rs::execute_transactions` (~L1265) — that loop IGNORES the per-tx return value and
already tolerates receipts appearing only at finish(). Validation runs on all 4 validators vs building on
1, so this still captures ~4/5 of network execution work. Verification on the fleet = the chain itself:
all 4 validators must agree on the payment-lane state root every height (divergence halts consensus =
loud safe failure). Full plan: experiments/reth-fork/README.md.
**🎯 SIGNATURE RECOVERY IS THE REAL BOTTLENECK (2026-08-08, MISSION-EL iter 5).** reth already
exposes the split — `transaction_execution_histogram` vs `transaction_wait_histogram` — so this
needed NO code/rebuild. Live, 146 blocks / 870k txs @ ~5,962 txs/blk: execute_transactions loop
15.48 us/tx = **wait-for-next-tx 10.18 (66%, SIG RECOVERY)** + executor 3.61 (23%) + loop overhead
1.69 (11%). So four iterations of executor optimisation (fast path, read caches, commit-once) got
our share to 3.61 us/tx and there is little left inside ArcBlockExecutor. ROOT CAUSE:
`payload_validator.rs:323` `let convert = |tx| tx.try_into_recovered();` — reth re-derives the
signer for EVERY tx in a payload and never consults the mempool, so each validator repeats ~47,618
ECDSA recoveries per block for txs it already recovered at pool insertion. NEXT LEVER: reuse
mempool-recovered senders (EL-side, needs the reth fork; up to ~10 us/tx = more than everything
gained so far). A wrong sender moves the state root, so the gates + live agreement catch errors.
**COMMIT-ONCE-PER-BLOCK (2026-08-08, MISSION-EL iter 4).** Per-block write overlay replaces per-tx
`db.commit` (which was 51% of fast-path cost). Verified bundle-identical — plain state AND REVERTS —
by a safety probe before writing executor code (a revert bug breaks reorgs without moving the state
root, so the gates cannot catch it). Offline executor path cumulative: stock 107.7/94.9 -> fastpath
64.1/58.4 -> +caches 54.1/45.4 -> **+overlay 45.3/35.3 ms = 2.23x/2.51x vs stock**. Live: 123 heights
zero divergence, exec 16.7 us/tx, block rate 1.33 -> 1.61 blk/s. KEY GAP: offline the executor is
~0.8 us/tx but live exec is ~16.7 us/tx => **~95% of live execution cost is OUTSIDE the executor**
(reth state provider + engine-tree loop). Micro-optimising inside ArcBlockExecutor is now near
exhausted; measure that gap next.
**EXECUTOR COST DECOMPOSITION (2026-08-08, MISSION-EL iter 3) — measure before optimising paid off
twice.** After the fast path + caches the ~54 ms executor path (47,618 transfers) splits:
**db.commit() per tx 27.8 ms (51%)** | receipts+misc ~14.5 ms (27%) | 2 state reads 7.1 ms (13%) |
arithmetic 4.6 ms (9%). CONSEQUENCE: the planned ~250-line BATCH EXECUTION change was DEPRIORITISED
— its prefetch only attacks the 13%. The real target is committing ONCE PER BLOCK instead of per tx
(~143k TransitionAccount records -> ~16k). Probe is in `parallel_transfer_bench.rs` ("decomposition"
block) so it is re-checkable. Design + revert-semantics caveat in experiments/reth-fork/MISSION-EL.md.
**PER-BLOCK READ CACHES (2026-08-08, MISSION-EL iter 2).** ~85% of live exec cost is state reads;
3 of the 5 a transfer does are block-constant. Cached in ArcBlockExecutor: blocklist status (map)
+ fee beneficiary (full AccountInfo tracked in memory, NOT just balance — a default would reset
nonce/code_hash and diverge the root). Both invalidated on any general-EVM tx. **5 reads/tx -> 2.**
Offline executor path stock->fastpath->+caches: pool 107.7->64.1->54.1ms, closed 94.9->58.4->45.4ms
(~2.0x vs stock). Live: 122 heights zero divergence, exec 17.6 us/tx @5,079 txs/blk. No batching
needed for this win; batch execution remains the next step (unlocks parallel prefetch + the
verified 4.25x sender-partitioned scheme).
**🔬 WHY FASTER EXECUTION DID NOT RAISE TPS — MEASURED, DEFINITIVE (2026-08-08).** Question: exec
improved 37% but fleet tps was flat; do big blocks degrade it? ANSWER: **no — big blocks are neutral,
and EXECUTION IS ONLY 10% OF BLOCK TIME.** Evidence: (a) tps is FLAT across an 8x block-size range
(4,098 tx @1.65 blk/s = 6.8k tps; 8,916 @0.82 = 7.3k; 34,245 @0.28 = 9.7k) — cadence falls in
proportion to size, so tps neither gains nor degrades; that is the signature of a PER-TRANSACTION
serialized cost, not per-block overhead. (b) CLEAN same-fleet block-size sweep (runtime gas-limit flip, identical load — the earlier
'flat tps' claim mixed single-machine and fleet runs and was NOT clean): **100M -> 1 Ggas (7.2x
bigger blocks) gives tps 7,196 -> 9,690 (+35%) while block latency goes 662ms -> 3,571ms (6.6x
WORSE)**. So bigger blocks are a modest THROUGHPUT win (per-block fixed costs amortize) and a large
LATENCY loss. They do not degrade tps — but the +35% is far short of the 7.2x size increase because
the dominant cost (SSZ encode + proposal streaming + voting on a ~6MB payload) scales LINEARLY with
tx count, exactly as expected. (c) Direct split at FULL 1-Ggas blocks: reth's own
newPayload handling = **357 ms** on a 47,618-tx block while block time = **3,750 ms** →
**EL 10%, everything outside the EL 90%** (per-tx: 78.8 us budget, 7.5 us in the EL, 71.3 us
outside). The 90% is CL/consensus: proposer getPayload for BOTH lanes, SSZ encode + proposal
streaming of a ~6 MB payload to 3 machines, decode, prevote/precommit round trips — and it scales
with block size, which is exactly why bigger blocks buy nothing. **CONSEQUENCE: even INSTANT
execution would raise tps only ~10%. Execution is DONE as a lever; the next lever is the consensus
coordination path (payload streaming/encoding + voting), not the EVM.**
**✅ MISSION COMPLETE — PARALLEL/FAST-PATH PAYMENT LANE LIVE ON ALL 4 MACHINES (2026-08-08).**
Fleet at FULL 1-Ggas blocks: **120 consecutive heights, ZERO divergence** (identical stateRoot AND
block hash on all 4 PHYSICAL machines); single-machine: 201 heights, zero divergence. Perf, apples
to apples vs the recorded stock fleet baseline (near-identical block composition 34,363 vs 34,245
txs): **exec 379ms -> 239.6ms/blk = 11.03 -> 7.00 us/tx = 1.58x, -37%**; state-root 4.7ms, persist
116.4ms, 9.7k tps avg / 14.7k peak. tps barely moved because cadence is CONSENSUS-COORDINATION-bound
(~2.9s/block vs ~0.36s of measured work) — execution is no longer the fleet bottleneck; coordination
is. Deploy recipe: `make build-docker` -> `fleet/ship-images.sh` (verifies the sha256 of the BINARY
inside the image; docker image IDs differ across daemon versions even when content is identical —
that produced a false MISMATCH) -> `PAY_GAS=1000000000 EXTRA_ACCOUNTS=16000
PAY_EL_ENV='-e ARC_PARALLEL_TRANSFERS=1' fleet/demo-fleet-metamask.sh start`.
**🎯 NATIVE-TRANSFER FAST PATH — IMPLEMENTED, VERIFIED, RUNNING (2026-08-08, commits 8225038 +
37ca3cc).** `ARC_PARALLEL_TRANSFERS=1` makes `ArcBlockExecutor::execute_transaction_without_commit`
return a hand-built `ResultAndState` for plain transfers instead of invoking revm; `commit_transaction`
/`finish()` are UNTOUCHED so receipts/gas/bloom stay production code, and the BUILDER path is unaffected
(it inspects per-tx results, which are identical). NO reth fork needed. Gate: empty input/access-list/
auth-list, TxKind::Call, gas_limit>=21k, sender+recipient have no code, nonce matches, funds suffice;
anything else falls through to revm. Arc's blocklist reads are replicated. Key detail: `Account::from(pre)`
seeds `original_info` (revm's bundle diff needs the PRE-state), then `info`=post + `mark_touch()`.
VERIFICATION (both gates must pass for any change here): `cargo run --release -p arc-evm --example
parallel_transfer_bench` AND the same with `ARC_PARALLEL_TRANSFERS=1` must print IDENTICAL — the second
form works because the bench's `run_serial` uses the REAL ArcBlockExecutor while `run_parallel`
(direct EVM) stays the oracle. LIVE RESULT on the 4-validator demo at 1 Ggas: ran ~820 blocks with ZERO
divergence, root+block-hash identical across all 4 at h203. Clean two-run A/B
(`experiments/dual-el/ab-fastpath.sh`): **per-tx exec 22.64us -> 16.33us = 1.39x**, all 4 agreeing in
both runs. Offline the executor path is 1.66-1.80x (106->64ms); the standalone arithmetic is 24x
(4.7ms) — so the REMAINING cost is receipt building + State/bundle commit, NOT the EVM. That is the
next optimization target. Env reaches containers via `PAY_EL_ENV='-e ARC_PARALLEL_TRANSFERS=1'`
(wired into launch-payment-els.sh + fleet payment_el_cmd). GOTCHA: never A/B by recreating a payment EL
mid-run — triggers finding #7 (EL loses unpersisted blocks, CL ahead, no backfill; heal =
`docker restart validator<n>_cl`, slow). Use two separate runs.
**RETH FORK STOOD UP (2026-08-07):** `~/reth-fork` (cp of `~/reth-2.3-ref`, v2.3.0). `experiments/reth-fork/apply-fork.sh`
appends a `[patch."https://github.com/paradigmxyz/reth"]` section (41 crates → absolute local paths) to
Cargo.toml — DEV-BOX ONLY, never commit (breaks Docker/CI); `apply-fork.sh revert` is a verified clean
round-trip (Cargo.toml==HEAD). Full `arc-node-execution` binary builds against the fork (10m30s).
TWO LEVERS, both located: (1) **state-root machinery = stock CLI flag `--engine.state-root-fallback`**
→ `StateRootStrategy::Synchronous` → `spawn_cache_exclusive` (NO multiproof task / prewarm / receipt-
stream; only `StateRootTask` strategy spawns those, gated in payload_validator.rs `spawn_payload_processor`).
NO FORK NEEDED for this half — test it on the payment EL first. (2) **parallel exec** = fork edit to
`reth-fork/.../engine/tree/src/tree/payload_validator.rs` `execute_transactions` (~L1265, the
`executor.execute_transaction(tx)` loop). Parallel scheme MUST: defer coinbase (Arc credits beneficiary
FULL fee every tx, base not burned — else all-tx conflict), cache blocklist bitmap (2 SLOADs/transfer on
NATIVE_COIN_CONTROL_ADDRESS), handle hot-recipient conflicts. Full plan + status: experiments/reth-fork/README.md.
NEXT (in order): measure state-root-fallback (no fork) → write parallel path → differential replay →
docker-from-fork (host-build+COPY; docker can't reach abs host paths) → fleet re-measure at 1 Ggas.
**FLEET A/B AT FULL 1-Ggas BLOCKS (2026-08-08):** val1 recreated live with the flag (node-local →
no chain restart; re-mesh val1 via admin_addPeer after any pay-EL recreate — runtime peer list is
lost). 47,618-tx blocks, 4 physical machines: val1 SYNC exec 304.7 + root 32.2 = 337ms vs val1's own
prior stock fleet baseline 379ms (~-20%, hw-consistent); cross-validator comparisons on the fleet are
CONFOUNDED BY HARDWARE (stock val4/alien2 280ms < stock val3/papaduck 623ms on identical blocks!) —
only same-machine A/Bs are valid. KEY HONEST POINT: fleet tps did NOT move (~0.35 blk/s, 2.9s/block vs
~0.5s of work) because cadence is CONSENSUS-COORDINATION-BOUND — root-machinery removal and even
future parallel exec show up in per-block exec_ms, not fleet tps, until the ~2.4s/height coordination
overhead is attacked. USER-RUN GOTCHAS (2026-08-08): plain `demo-fleet-metamask.sh start` = NO flag
(must pass PAY_EL_EXTRA_ARGS) and EXTRA_ACCOUNTS defaults to 1000 (S=1 ACCTS=250 for distributed spam);
"root field still changes" is EXPECTED — the flag changes HOW the root is computed, not WHETHER
(no-root = pending fork edit).
**LEVER 1 MEASURED LIVE (2026-08-07, commit 34d9fec):** single-machine 1 Ggas demo, val1 pay-EL with
`--engine.state-root-fallback` vs val2-4 stock, 333 IDENTICAL blocks (~3.4k tx each): val1 exec
**67.4ms/blk + root 12.3 = 79.7 total** vs stock **104.5 + 2.2 = 106.7** → async root machinery taxes
exec by **~35%**; net **-25% total work**, zero code changes, mixed config kept consensus (A/B via new
`PAY_EL<i>_EXTRA_ARGS` env in launch-payment-els.sh). Execution now dominates (67 vs 12) → parallel
exec in the fork is the remaining lever. Parallel execution NOT YET IMPLEMENTED (fork builds, hook
located, no parallel code written). Note: sync root 12.3ms > the 0.8ms fleet number because this box
ran 8 spammers + 12 containers concurrently (contention), and async "root 2.2ms" hides its real cost
inside the exec window (workers overlap) — the honest comparison is the TOTAL column.

Other branches: `gravity-payment-lane` PARKED (gravity-reth = O(total-state) per block on standard
Engine API paths, perf requires their consensus; FINDINGS.md there); erigon probe passed the engine
smoke (needs Prague system-contract predeploys in genesis; chainId 1337 collides with its named
bor-devnet chain — don't pass --networkid) but was abandoned after a box crash. TODO: cherry-pick
demo-bloat fail-loud preseed (09d02dd) to dual-el-payment-lane; presentation deck (paper repo
presentation/slides.tex) is demo-driven and carries all these numbers.
