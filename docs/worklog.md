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
