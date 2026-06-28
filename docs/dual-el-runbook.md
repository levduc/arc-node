# Dual-EL payment-lane testnet — reproducible runbook

A 4-validator Malachite BFT testnet where **each node runs two reth execution layers** — an EVM
lane and a payment lane — and **every consensus block carries two payloads (two state roots)**. The
consensus layer (CL) builds, validates, and finalizes **both** ELs every block; all validators
re-execute both lanes and must agree on both roots.

Branch: **`dual-el-payment-lane`**.

```
   Malachite BFT (4 validators): ordered heights, one commit certificate per height
            │                         │
   ConsensusBlock(N)          ConsensusBlock(N+1)
   ├ height/round/proposer    ├ …
   ├ execution_payload ─parentHash→ execution_payload     ← EVM lane (its own hash chain)
   ├ payment_payload   ─parentHash→ payment_payload       ← payment lane (its own hash chain)
   └ proposer signature       └ …
```
Each lane is an independent hash chain; the certified consensus value is the EVM block hash; the
payment payload is co-streamed and co-validated (agreed by every validator) each height.

---

## 1. Prerequisites (host)
- Linux, Docker (with compose v2), ~8 CPU cores, ≥16 GB RAM, ≥80 GB free disk (more for long soaks).
- **Node.js 22** via nvm (Node 18 breaks the genesis step — `HH19`/`ERR_REQUIRE_ESM`).
- **Rust** per `rust-toolchain.toml` (rustup).
- **clang + libclang-dev** (reth-mdbx-sys bindgen needs them).
- **Foundry** pinned in `.foundry-version`.

```bash
nvm install 22 && nvm use 22
# rustup picks up rust-toolchain.toml automatically
sudo apt-get install -y clang libclang-dev
```

## 2. One-time repo setup (already committed on the branch)
- `Cargo.toml` reth deps point at upstream **`tag = "v2.3.0"`** (NOT a local `file://` fork — the fork
  can't be reached inside the Docker build). If you see `file:///…/reth-2.3-ref`, revert to the tag.
- `assets/localdev/genesis.config.ts`: `blockGasLimit = 100_000_000` (payment-lane spec; both lanes
  share this genesis), `targetBlockTimeMs = 250`.
- `assets/localdev/payment-jwt.hex`: shared JWT for the payment-lane Engine API. Regenerate if missing:
  ```bash
  openssl rand -hex 32 > assets/localdev/payment-jwt.hex
  ```

## 3. Build (Docker images + quake)
The CL image carries the dual-EL consensus code; the EL image is the standard arc execution client
(used for **both** lanes). Build with the fast `dev` profile:
```bash
make build-docker            # builds arc_execution:latest + arc_consensus:latest (BUILD_PROFILE=dev)
# (or build only the CL after a CL-only change:)
# DOCKER_BUILDKIT=1 docker compose -f deployments/arc_consensus.yaml build --build-arg BUILD_PROFILE=dev
cargo build --bin quake      # quake embeds the dual-EL compose template (templates/local/compose.yaml.hbs)
```

## 4. Start the testnet (4 × CL + EVM-EL)
```bash
nvm use 22
cargo run --bin quake -- -f crates/quake/scenarios/soak4.toml start -e 1000 --monitoring false
```
- `soak4.toml` is a 4-validator scenario (a copy of `localdev4.toml`); the testnet dir is `.quake/soak4`.
- `--monitoring false` skips blockscout (it's not needed and its leftovers can interfere — see Gotchas).
- EVM ELs publish host ports **8545 / 8645 / 8745 / 8845** (http), `+1` ws, `+6` authrpc. CLs RPC on 31000+.

## 5. Add the payment EL per validator
```bash
cp assets/localdev/payment-jwt.hex .quake/soak4/assets/
TESTNET=soak4 bash experiments/dual-el/launch-payment-els.sh
```
- Launches a 2nd reth EL per validator (`validatorN_el_pay`) on the **same docker network**
  (`arc_testnet_default`) so each CL reaches its payment EL by name, plus `host-access` for host RPC.
- Payment ELs publish host ports **19545 / 19645 / 19745 / 19845** (http), `+1` ws, `+6` authrpc.
- Each CL was generated (by the compose template) with `--payment-execution-endpoint=http://NODE_el_pay:8551`
  etc., and connects automatically (it retries until the payment EL is up). Within seconds both lanes
  advance in lockstep.

## 6. Verify
```bash
# both lanes advancing, in lockstep:
for p in 8645 19645; do cast block-number --rpc-url http://127.0.0.1:$p; done

# differential agreement (payment block hash identical across validators at a settled height H):
H=$(($(cast block-number --rpc-url http://127.0.0.1:19645)-10))
for p in 19545 19645 19745 19845; do cast block $H --rpc-url http://127.0.0.1:$p --json | jq -r .hash; done
# all four must print the same hash

# two roots in one block (drive the payment lane only, then compare a block's roots):
cast block <H> --rpc-url http://127.0.0.1:8645  --json | jq -r .stateRoot   # evmStateRoot
cast block <H> --rpc-url http://127.0.0.1:19645 --json | jq -r .stateRoot   # paymentRoot
```

## 7. Drive load (spammer)
```bash
# EVM lane:
target/release/spammer ws --targets ws://127.0.0.1:8646,ws://127.0.0.1:8746,ws://127.0.0.1:8846 \
  -r 300 -t 60 -a 500 -g 4 --mix transfer=100
# payment lane:
target/release/spammer ws --targets ws://127.0.0.1:19646,ws://127.0.0.1:19746,ws://127.0.0.1:19846 \
  -r 300 -t 60 -a 500 -g 4 --mix transfer=100
# fill blocks to the 100M gas limit (high-gas txs):
#   --mix guzzler=100 --guzzler-fn-weights "hash-loop=100@2000"
```

## 8. Soak / correctness monitor
```bash
DUR=3600 RATE=300 bash experiments/dual-el/soak.sh   # 1h; set DUR=86400 for 24h
# checks every 60s: liveness (each lane advances), agreement (block hash identical across validators
# per lane), health (12/12 containers), disk; runs continuous dual-lane spam; prints PASS/FAIL.
```

## 9. Teardown
```bash
docker ps -aq --filter "name=validator" | xargs -r docker rm -f
docker run --rm -v "$PWD/.quake":/q alpine rm -rf /q/soak4   # datadirs are root-owned (container)
```

---

## Gotchas (and the machine-specifics seen on the dev box)
- **Port 8545 taken by a stray `anvil`** (chainId 31337): if any process holds host `:8545`, then
  `validator1_el` can't publish its host port and `cast …:8545` hits anvil instead (looked like
  "validator1 stuck at block 18"). The node is fine — query it over the docker network:
  ```bash
  docker run --rm --network arc_testnet_default curlimages/curl -s -X POST http://validator1_el:8545 \
    -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'
  ```
  On a clean machine with nothing on 8545, all four EVM ELs publish host ports normally.
- **Stale blockscout containers**: a prior `make testnet` *with* monitoring leaves blockscout
  containers bind-mounting `.quake/<name>/blockscout/*`; a crash-looping `db` recreates `.quake/<name>`
  as root and blocks a fresh `quake start`. Avoid by using a fresh scenario name and `--monitoring false`;
  if hit, stop those containers (`docker stop backend db`).
- **Root-owned `.quake/<name>`**: EL containers create datadirs as root, so clear via a throwaway
  container: `docker run --rm -v "$PWD/.quake":/q alpine rm -rf /q/<name>`.
- **Payment lane builds tiny blocks** unless its EL has builder flags — `launch-payment-els.sh` passes
  `--arc.builder.deadline=2000 --arc.builder.wait-for-payload=true` so it fills blocks (and large
  txpool caps so it can hold a full mempool).
- **CL waits for the payment EL at startup** (retries forever). Always run step 5 after step 4; the CLs
  unblock as soon as the payment ELs are up.
- **v0 limitation**: value-sync (catch-up of a late-joining node) carries only the EVM payload; the
  live proposal path carries both lanes. Start all 4 validators together from genesis.
- **Consensus value commits to the EVM hash only** (payment root is co-signed in the proposal, not
  folded into the certified value). Folding `paymentRoot` into the value/header is a planned hardening.
