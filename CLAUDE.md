# CLAUDE.md — arc-node working notes

Arc is an EVM-compatible L1: a **customized Reth** execution layer driven by the **Malachite**
BFT consensus layer over the Engine API. The work tracked here adds a second, **lean payment
lane** that runs beside the EVM lane under the same consensus.

**This repo is the archive and the worklog** (`docs/worklog.md`, `docs/campaign-log.md`,
`docs/history/`), not the code. This file is the working document: current state, how to run
it, what not to repeat. The live code:

| what | where | notes |
|---|---|---|
| CL delta (the port) | arc branch **`lean-lane`**, worktree `~/arc-lean-clean` | clean 8-commit series on origin/main 97f8da0 (reth v2.2.0); guide `docs/lean-lane-integration.md` |
| CL + height timing | arc branch `lean-lane-instrumented` | `lean-lane` + one commit, for bench runs only |
| lean node | lean-lane branch **`v0.3-dev`**, worktree `~/lean-lane-v03` | reth v2.3.0, no fork; contract `docs/integration.md`; README has every flag |
| fleet/bench tooling | lean-lane `bench/` | runner `bench/fleet/fleet`, sampler, `analysis/heights.py`; numbers in `bench/README.md` |
| archives | arc `lean-lane-v0.2`, lean-lane `v0.2` | frozen; revert points `lean-lane-v0.2-lastgood` / `v0.2-lastgood`; gates tagged `gate-<phase>-<date>` |
| this repo | `crates/lean-*`, `experiments/` | diverged **v0.1 snapshot** and campaign scripts — history only |

## 1. Toolchain

- **Rust** per `rust-toolchain.toml`, via rustup (each repo pins its own).
- **clang + libclang-dev** — reth-mdbx bindgen needs them (arc only; the lean node has no C deps).
- **Docker** for arc images and every testnet (CL+EL containers).
- **Foundry** pinned in `.foundry-version` (`cast` — governance txs, fund-file generation).
- **Node ≥20.19 / 22.x (even majors)** — genesis step only. Node 18 fails with a misleading `HH19`.
- **A fresh arc worktree needs `git submodule update --init --recursive`** — the genesis contract
  compile fails without it.

## 2. Common commands

```bash
# arc (~/arc-lean-clean)
make build / make build-docker        # node / CL+EL images
make genesis                          # assets/localdev/genesis.json (idempotent)
make testnet / testnet-down / -clean  # stock quake testnet (5 validators)
make test-unit                        # rust unit tests + lint
# lean-lane (~/lean-lane-v03)
cargo build --release -p lean-lane-node -p spammer
```

`quake` is the in-repo testnet manager (`crates/quake`), run via the Makefile, not a global
binary. Local RPCs for a running testnet: `.quake/localdev/nodes.json`.

**Crate-name landmine:** the CL crate is `arc-node-consensus`. `cargo check -p
arc-malachitebft-app` silently checks the *git* dependency and prints "Finished" having done
nothing — it masked 8 real errors once. Always `-p arc-node-consensus`.

## 3. The two lanes (v0.2 design, unchanged on `lean-lane` / `v0.3-dev`)

| | EVM lane | lean payment lane |
|---|---|---|
| client | customized reth (arc) | `lean-lane-node` (lean-lane repo) |
| chain id | 1337 (localdev) | 1338 |
| interface to CL | Engine API (IPC) | 5-verb JSON-RPC shim, commitment-addressed |
| state | full MPT, receipts, logs | flat nonce+balance map, keccak commitment chain |
| tx | EIP-1559 native transfer, 122 B | fan-out type `0x50`, 72 + 28N B |
| enabled by | always | `ARC_PAYMENT_LEAN_LANE=1` on all validators from height 1 (off ⇒ upstream, byte-identical) |

**One certificate, one voted value.** Consensus votes the **plain EVM block hash**, as upstream;
the lean block is bound by the EVM header's **`prev_randao` = lean commitment** (zero with the
lane off). There is no composite value id (that was v0.1). Lean bytes ride beside the EVM
payload in proposals and value-sync (`[u64 len|LEAN_LANE_BIT][evm SSZ][lean]`, framed by the
node's flag, fails closed on mixed flags).

**Shim verbs:** `arc_buildBlock`, `arc_stageBlock`, `arc_newBlock{blockBytes|commitment}`,
`arc_getHead`, `arc_getBlockBytes{number|commitment}`; lag is `SYNCING`, never an error. Lean
nodes gossip to each other with `arc_announceBlock` (announce + backfill pull).

**Voting is structural:** resolve (header names a block with no bytes ⇒ must exist locally),
binding (`prev_randao` = recomputed commitment), timestamp lockstep, parent linkage (with a
bounded 5 s peer catch-up), then fire-and-forget stage. Verdict `Valid | Invalid | Abstain` —
being behind abstains, never `Invalid`. Execution happens in the vote gap; decide anchors **by
commitment** (the node already holds the bytes). A lane-on block with no lean commitment is
`Invalid` (ede2651 — halt fix, not backported to the v0.2 archive). Total-STF semantics (an
invalid tx is a no-op) mean a byzantine proposer can waste bytes but cannot halt or fork the lane.

## 4. Running it

**Verify a machine** (~1 min, no docker/fleet): lean-lane `./scripts/lean-smoke.sh` — three lean
nodes on loopback, stand-in CL, signed fan-out txs, convergence + replay. Last run: 40 heights,
9,856 txs, 98,560 payments, 3/3 at one commitment. (`experiments/dual-el/lean-smoke.sh` here
runs the v0.1 snapshot.)

**Local 5-validator testnet** (arc branch `lean-lane`, lean-lane checkout beside it or
`LEAN_LANE_DIR=…`):

```bash
make testnet-lean                       # images, 5 host lean nodes, quake localdev-lean.toml
make testnet-lean-load LOAD_SECS=180    # LOAD_RATE, FANOUT, POOL_TARGET
make testnet-lean-status                # heights, head fullness, lean agreement
scripts/lean-testnet.sh restart 3       # restart one CL; its lean node stays up
make testnet-lean-down
```

`lean-testnet.sh up` wipes `.quake/lean-nodes/` — never keep a log there.

**Fleet (4-machine LAN)** — lean-lane `bench/fleet/fleet`, recipe in `bench/README.md`:

```bash
cp bench/fleet/fleet.env.example bench/fleet/fleet.env   # IPs, user, ARC_DIR (git-ignored)
BUDGET_GAS=400000000 FANOUT=100 bench/fleet/fleet up     # ship, boot, health gate
LOAD_SECS=600 bench/fleet/fleet load                     # load + per-minute samples
bench/fleet/fleet down                                   # logs pulled, data kept
bench/analysis/heights.py proposers bench/runs/<run-id>
# write the run record (worklog), then: bench/fleet/fleet purge <run-id> --yes
```

`LANE=evm` is the EVM arm on the same fleet (stock CL, gas limit at genesis, verified off a
produced block). **Extend this runner rather than hand-rolling per-experiment shell** — every
gate in it was paid for with a wrong number. `experiments/dual-el/history/lane-bench.sh` and
`deploy-lean.sh` are its v0.1 predecessors, kept as history.

**Genesis funding** — every lean node needs the same fund file, exactly the accounts the spammer
signs with, derivation `m/44'/60'/1'/0/i` (the **`1'`**): lean-lane `scripts/gen-lean-fund.sh 800
~/lean-fund.txt`. `--fund-balance` is u64 and near max: scale accounts, not balance.

**21 nodes on AWS** go through quake's remote mode (Phase 4), not the LAN runner.

## 5. Where things stand (4-machine fleet, 500 ms pacer, one spammer per machine)

All lean rows: every sampled block **100 % of budget**, 0 CL restarts, 4/4 byte-identical at the
end; 10-min windows unless noted; n=1 per point, ±15 % run-to-run.

| config | stack | blk/s | payments/s |
|---|---|---|---|
| lean N=100, 300 M (F7) | v0.2-perf | 1.86–1.92 | 106,914–110,437 |
| lean N=100, 350 M (F9) / 30-min soak (F14) | v0.2-perf | 1.79–1.83 / mean 1.75 | 120,354–122,667 / 109,038–126,744 |
| **lean N=100, 400 M (F26)** | v0.2 final | **1.86–1.97** | **142,443–150,965** (staged anchors 1244/1283) |
| lean N=100, 400 M, 30-min soak (F28) | v0.2 final | 1.57–1.99, mean ~1.78 | 120,529–152,183; 28/28 min > 120k |
| lean N=100, 450 M (F27) | v0.2 final | 1.91 → 1.64 sliding | 141,586–164,509 |
| **lean N=100, 400 M (run-0924-1421)** | **clean `lean-lane` + `v0.3-dev`** | **1.90–1.93** | **145,730–148,287** (staged 94–100 %, lean RSS 180–300 MB) |
| lean N=1, 150 / 300 / 450 / 600 M (F20–F23) | v0.2-perf | 1.90–1.95 / 1.92–1.97 / 1.81–1.84 / 1.43–1.48 | 11.0–11.3k / 22.2–22.7k / 31.3–31.9k / 33.0–34.3k tx/s |
| EVM N=1, 150 M / 300 M (E150/E300) | stock CL | 0.8–1.57 / 0.68–0.81 | ~6.5k / ~6.4k tx/s mean, **51–78 % / 50–69 % full — never full** |

Knee at the 2 blk/s target: **400–450 M** at N=100 (450 M slides within 10 min), 450 M at N=1.
Above it the fleet is bound by ~4.4 MB/s of block bytes through consensus, not by signatures
(N=200 = N=100) or chunk size.

### Durable findings

1. **Execution is not the bottleneck.** The lean node imports at ~1 µs/output; a staged anchor
   costs 8–11 ms at 400 M. The EL was 7–15 % of a height on both lanes (v0.1).
2. **Consensus transport is.** v0.1 decomposition at drain size: proposal stream+assemble 57 %,
   votes 13 %, anchor 10 %. v0.2 at 300 M (F7): first part 115–150 ms, stream 116–129, votes
   60–120, decided→anchor 76–98, period ~530 ⇒ pacer-bound with ~170 ms slack.
3. **At one payment per signature the lanes are NOT equals** (retracted 2026-09-15). The EVM lane
   saturates at ~6.5 k tx/s ≈ 0.14 Ggas/s at any gas limit with blocks never full
   (execution/builder-bound); the lean lane at N=1 is pacer-bound at 100 % full (11.3k/22.7k) and
   does ~32k at the knee. The old "~6 k on both" measured a delivery-starved spammer.
4. **Fan-out buys the rest**: ~20× the EVM lane's payments at N=100 (142–151k vs ~6.5k), ~4.5×
   lean N=1 at the same byte wall. It amortises per-*transaction* costs (signature, pool entry,
   envelope): per 100 payments 100→1 signatures, 10,000→2,872 wire bytes; execution unchanged
   but conflict-free by construction.
5. **Nothing per-signature or O(chain) on the node's serving/runtime path.** Serial ecrecover
   starved admission (0.63 vs 1.86 blk/s with rayon; always parallel in v0.3). A backward log
   scan on `arc_getBlockBytes{commitment}` caused a 10-minute cadence drift (100 → 57 blk/min)
   invisible in short windows — measure ≥ 10 min. The CL's staged block can arrive *after* its
   anchor on a LAN (loopback cannot show it): a 60 ms anchor grace took peer-pulls 515 → 16.
   Pool: reth's per-sender slot cap nonce-poisoned deep senders; `max_account_slots` 4096.

## 6. Measurement protocol (why every number needs it)

A validator can look perfectly healthy — containers up, agreement passing, blocks 100 % full —
while contributing **nothing**: a **parked CL** (booted while its lean node was down → "Manual
intervention required"), or a CL degenerated into a **pure sync-follower** (decides by fetching,
never proposes). Each costs ~25 % of cadence and is invisible to agreement checks. **Always
attribute failed round-0s by proposer** (`heights.py proposers`).

Baked into `bench/fleet`, and mandatory for any hand-run experiment:

- **health gate** after boot: containers up, unparked, EL fresh, zero CL restarts, chain advancing;
- **lean nodes before CLs**, lane on from height 1 of a fresh chain;
- **fullness with every number** — a non-full block measures delivery, not the chain;
- **one spammer per machine** on loopback, disjoint accounts (a remote one is delivery-bound);
- **per-validator restarts, pool depth, `staged%`** every minute (reference ≥ 95 % staged);
- **chain-static check** before `-l` corpora (a surviving feeder advances nonces → corpus born
  stale → queued forever); the lean `eth_getTransactionCount` returns the committed nonce, so
  a corpus generated over a non-empty pool restarts at already-pooled nonces — empty pools only;
- **logs captured before teardown**; **remote effects verified by reading them back** (ssh exit
  codes lie once the tailscale session expires); sample from an **uninvolved wired validator**.

### Ops landmines (each cost a run)

- **tailscale ssh expires after ~4–5 h** and only the user can re-auth: they run a waiting command
  per machine (`tailscale ssh <host> true` prints a login URL and blocks until approved). Rows
  with restarts `-1` are invalid; after re-auth, `fleet down` the orphaned run before anything.
  Queue the important runs first. The preflight refuses a relayed (DERP) path — wait it out.
- **Run dirs hold root-owned files** (container data): delete only via `fleet purge` (root
  container) — plain `rm -rf` leaves GBs behind. **Write the run record before purging.**
- **Two chains sharing a run id**: RUN_ID is minute-granular; an orphaned subshell started a run
  in the same minute as a relaunch and its `down` killed the live one. Kill orphans, or set
  `RUN_ID` explicitly.
- Old stopped containers can reference a deleted docker network — remove them before `up`.
- `pkill`/`pgrep -f` matches the shell carrying the pattern; `pkill` returning 1 aborts a
  compound block. Split kill and launch into separate calls; guard with `|| true`.
- Remote background launches need `(setsid nohup … &)` + `</dev/null` (env vars *before*
  `nohup`); bash bare `wait` blocks on *any* job — `disown` daemons, wait on explicit PIDs.
- zsh aborts a compound command on a no-match glob (`setopt nonomatch` or enumerate), and does
  **not** word-split `set -- $var` (a spammer once got `-r "4000 8000 600"`).
- RPC bodies go through **files, never argv** — a budget-full block's base64 is MBs.
- Fan-out **drains sender balances one-way** (bankruptcy ~4.4 k txs at N=100): N=100 runs with 800
  accounts stay ≤ 30 min; refund by regenerating genesis.
- Never wipe a lean chain under a live CL chain: certificates bind destroyed bytes and laggards
  can never sync that span. Enable the lane only from height 1 (the enable-boundary height is
  unserveable); change CL flags on ALL CLs together (halt-flip-resume — a lone CL restart
  wedged: sync deadlock + ±128 serving window). Consensus params (pacer, timeouts) change live
  via governance.

## 7. Layout

- arc `lean-lane`: `crates/malachite-app/src/lean_lane/` (binding, catch-up, anchor),
  `crates/eth-engine/src/lean_shim.rs` (`LeanNode`), `crates/types/src/lean.rs` (framing),
  `scripts/lean-testnet.sh`, `docs/lean-lane-integration.md` (CL delta, porting order, config).
- lean-lane `v0.3-dev`: `crates/{lean-native,lean-lane-node,spammer}`, `docs/integration.md`,
  `docs/setup.md`, `scripts/` (smoke, fund, feeder, replay-bench), `bench/`.
- this repo: `docs/history/` (superseded guides, plans), `experiments/dual-el/history/`,
  `fleet/`, `fleet-v7/` (old runners), `2026.arc.payment.highway.claude/` (paper + notebook).

## 8. Session log — REQUIRED

Every session appends one dated entry to **`docs/worklog.md`** (this repo) before it ends —
the record that keeps the work reviewable remotely (`git log -p`), not a chat summary:

- **Changed** — what was edited/built, with commit hashes (and which repo/branch).
- **Measured** — numbers *with cadence and fullness*, and the config that produced them. A
  number without fullness is not a result (§6).
- **Broke / retracted** — failures, and earlier claims invalidated (the most valuable lines).
- **Decided** — design decisions taken, with the reason.
- **Open** — what the next session should pick up.

Mark speculation as **BRAINSTORM** so it is never mistaken for measurement. Durable conclusions
get promoted into this file (§5/§6); the worklog stays raw.

## 9. Scope — what is done, what is next

**Done.** v0.2 design (§3); self-healing lean nodes (announce/pull, SYNCING queue, backfill,
snapshots); §5. De-slop (plan `~/arc-lean-perf/.superpowers/deslop-plan.md`): Phase 0, Phase 1
(`lean-lane`, +3,388 lines vs upstream, was ~12,000; review CLEAN vs v0.2), Phase 2 items 1–6, 9
+ peer-pull race (`v0.3-dev`), Phase 3 (tooling → `bench/`, these docs); fleet parity.

**Not done / known limits.** Clean stack has a 10-min fleet run only, no soak; ~8 % late decline
over 30-min soaks (F10/F14/F28) unexplained. Four heterogeneous validators, one on wifi; n=1
per point. No comparison against a published system. No formal safety argument for the binding.

**Next, in the order I would do them:**

1. **Phase 2 rest** (lean node): item 7 collapse the concurrency to the commit gate + one arrival
   wait (fleet path-mix re-measure, staged ≥ 95 %), item 8 test consolidation, item 10 spammer →
   a small lean load tool; tag v0.3.0. Gate: tests, smoke, local e2e, fleet 400 M + 30-min soak.
2. **Phase 4 — 21 nodes on AWS via quake**: lean node image (ghcr), `[lean]` manifest section +
   lean service in both compose templates, load container per node, CL catch-up peer fan-out;
   ladder local 5 → local 10 → LAN parity → AWS 21 single-AZ → multi-region. Measure gossipsub
   `flood_publish` and the value-sync 10 MiB cap first; expect the knee to drop at 21.
3. **Light client Phase 1** — attested indexer over certified bytes (no protocol change).
4. Endurance: 60 min at 300 M with 1,600 accounts (drain limit) to chase the late decline.

**Deferred until measured:** reth-pool replacement (behind a differential test + soak); removing
the CL-side catch-up (fleet A/B: kill one lean node 60 s under load). **Structural lever still
open:** compact proposals (tx hashes against pre-distributed bodies — Narwhal's separation on an
existing BFT stack). **Parked** (measured, not promising): parallel execution; bigger blocks
(knee 400–450 M); packing below 28 B/payment (BRAINSTORM: index-encoded recipient ~12 B).
