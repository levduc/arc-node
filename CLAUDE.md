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
