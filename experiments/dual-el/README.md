# Dual-EL payment lane — step 2 (1 CL + 2 EL per node)

`launch-payment-els.sh` adds a **second reth EL ("payment lane")** per validator to a running
quake testnet: real reth (`arc_execution:latest`), own datadir (`<node>/reth-pay`), the genesis,
own ports (http 19545/19645/19745/19845, ws +1, authrpc +6), discovery off, on the
`arc_testnet_host-access` network. It runs idle at block 0 until the CL drives it (step 4) — this
is the "node boots with 2 ELs" milestone. No simulation.

## Use
```bash
# 1. start the base testnet (1 CL + 1 EVM-EL per validator)
cargo run --bin quake -- -f crates/quake/scenarios/localdev4.toml start -e 50000 --monitoring false
# 2. add the payment EL per validator
bash experiments/dual-el/launch-payment-els.sh
# 3. verify (payment ELs respond at block 0; EVM ELs advance)
cast block-number --rpc-url http://127.0.0.1:19645   # payment lane (idle)
cast block-number --rpc-url http://127.0.0.1:8645    # EVM lane (advancing)
```

## Status / next
- **Step 1 (CL drives a 2nd EL):** NOT done yet. The meaningful form is coupled to step 4 (the CL
  actually building/validating on EL2), and shipping it needs the consensus Docker image rebuilt —
  currently blocked because `Cargo.toml` points reth at a local `file://` fork the Docker build
  context can't reach. So the payment EL here is standalone (not yet CL-driven). Do step 1+3+4
  together: add a 2nd Engine to malachite-app, `ConsensusBlock.payment_payload`, proposer builds
  both, validators re-execute both. See `docs/dual-el-payment-lane.md`.
