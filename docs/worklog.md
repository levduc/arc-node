# Work log

Append-only, one entry per session, newest at the bottom. Convention in
`CLAUDE.md` §8. Numbers without cadence + fullness are not results.

---

## 2026-08-25 — teardown, repo cleanup, planning

**Changed**
- `fe5c2bf` CLAUDE.md rewritten as a working document (2,009 → 226 lines); full
  history preserved verbatim in `docs/campaign-log.md`.
- `a598120` `lean-smoke.sh` — single-machine proof the repo runs the lane alone.
- `f007c73` `fleet.env` + `deploy-lean.sh` (sha-verified *after* transfer).
- `1bdfd39`/`666c5ad` lean lane moved into this workspace as `crates/lean-native`
  + `crates/lean-lane-node`, building against upstream reth v2.3.0; setup guide.
- `docs/roadmap.md` — scope and plan (this session's main deliverable).
- Reverted an uncommitted `assets/localdev/genesis.json` — generated-file churn
  from a demo run with different `EXTRA_ACCOUNTS` (1,293 → 294 accounts), not an
  intentional change.

**Measured**
- `lean-smoke.sh`: 40 heights, 9,856 txs, 98,560 payments, 3/3 nodes at one
  commitment; second run 25 heights, 6,336 txs — repeatable.
- Repo-built lean binary reproduces the fork binary's genesis commitment
  `0xef24da0138bf3715…` — same wire format and semantics.
- Fleet torn down; ~2.4 TB reclaimed (papaduck 63 GB of it was 37 dangling arc
  images from image ships).

**Broke / retracted**
- Nothing new. Standing retractions from 2026-08-24 (sick-validator N=1
  diagnosis; non-byte-normalised N=1 row) are recorded in the notebook and
  `campaign-log.md`.

**Decided**
- The reth fork is history: nothing in this repo depends on `~/reth-fork`.
- Historical `experiments/dual-el/fleet/*` launchers stay hardcoded as campaign
  history; the live path is parameterised.

**Found while planning (new, unverified consequences)**
- **Beneficiary is node config, not block data** (`Address::with_last_byte(0xbe)`)
  — consensus-critical input outside the block; differing config = silent fork.
- **Invalid transactions pay no fee** — with total STF, a byzantine proposer can
  include invalid txs for free, costing every validator bytes + ecrecover.
- **Transactions do not propagate between lean nodes** — only blocks do. Every
  benchmark fed all nodes directly, which hid this.
- No cross-lane value flow exists: the payment lane is an island.

**Open**
- Decisions needed: checkpoint state root (yes/no/how often); header `version` +
  `proposer` fields; invalid-tx fee policy.
- Next up per `docs/roadmap.md`: beneficiary fix, then single-machine runner.

**Decisions (Duc, same day)**
- Checkpoint state root: **yes**, designed **lagged** (block N carries the root as
  of the last boundary ≤ N−K) so neither proposing nor voting waits on it;
  verification happens at execution, mismatch = attributable halt. Use an
  incremental accumulator, not an O(n) walk — the flat state grows with users.
- Header gains **`version` + `proposer`** (one wire-format break, done with the
  checkpoint field).
- Invalid-tx fee: **charge the proposer**; do not reject the block, do not halt.
- Payment lane **may run at a different cadence** from the EVM lane (product is
  fine with 1 or 0.5 blk/s for payments).

**BRAINSTORM — cadence analysis (not measured, see roadmap §7)**
- The lane is **pacer-bound, not capacity-bound** today: 150M→225M all sit at
  ~518 ms/1.93 blk/s; only 250M slips (538 ms). Free +7 % by moving to 250M.
- Local fit height ≈ 343 ms + 0.14 µs/B extrapolates 1 blk/s → 162 k payments/s,
  but that same law underpredicts the measured 525M drain (710 ms predicted vs
  **1,155 ms measured**, 1.6× off) — superlinearity above ~1.5 MB. Honest range
  for 1 blk/s: **87 k–160 k, most likely ~110 k**. Must be measured, not assumed.
- Agreement risk if cadence slows: **`propose` timeout is 3,000 ms and does not
  scale automatically** — at 3–4.6 MB payloads a round can miss it and cadence
  collapses. Timeouts are on-chain params; raise them with the cadence.
- Per-lane cadence needs no new consensus machinery (`lean_payload` is already
  `Option`, `commit_lanes(evm, None)` is the EVM-only case) — just a deterministic
  `height % K` proposer rule, plus timeouts sized for the expensive height.

**Open (next session)**
- Run the cadence experiment (roadmap §7.2) before building per-lane cadence.
