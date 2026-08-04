#!/usr/bin/env bash
# Two-lane demo v0, tuned for a MetaMask walkthrough (single machine, localhost).
#
#   EVM lane      chainId 1337   30M gas    RPC http://<host>:8545
#   Payment lane  chainId 1338   200M gas   RPC http://<host>:19545
#
# Both lanes are one chain: same 4 validators, same consensus round, one certificate.
# Distinct chainIds so MetaMask can hold them as two networks (two balances, one dropdown).
#
#   ./demo-metamask.sh start | stop | status | metamask
#
# Why 30M vs 200M: the EVM lane keeps mainnet-like limits; the payment lane is lean (transfers
# only, no contract storage) so it can safely run a much larger block. Arc reads the block gas
# limit from the ProtocolConfig contract at runtime, so both the header AND that contract slot are
# set -- the EVM lane via quake's --block-gas-limit, the payment lane by patching its genesis.
#
# Funded account for the wallet: import the standard dev mnemonic
#   test test test test test test test test test test test junk
# MetaMask's first account (0xf39Fd6e5...) is prefunded with 1,000,000 on BOTH lanes.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCEN="demo2lane"; PORT=8080; RUN="/tmp/dualel-metamask"
DASH="$REPO/experiments/dual-el/dashboard.py"
EVM_CHAINID=1337
PAY_CHAINID=1338
PAY_GAS=${PAY_GAS:-200000000}          # 0xBEBC200 (200M). Override e.g. PAY_GAS=1000000000 (1 Ggas).
EVM_GAS=${EVM_GAS:-30000000}           # 0x1C9C380 (30M)
EXTRA_ACCOUNTS=${EXTRA_ACCOUNTS:-1000} # prefunded genesis EOAs (raise for many parallel spammers)
BLOCK_TIME_MS=${BLOCK_TIME_MS:-500}    # Arc's product target: 2 blocks/s. Genesis ships 250ms; we
                                       # set 500 at runtime via ProtocolConfig (set-block-time.sh).
PROTO_CFG_ADDR="0x3600000000000000000000000000000000000001"
GAS_SLOT="0x668f09ce856848ead6cb1ddee963f15ef833cea8958030868f867aec84385203"
cd "$REPO"; mkdir -p "$RUN"

setup_env(){ export NVM_DIR="$HOME/.nvm"; [ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh" >/dev/null 2>&1; nvm use 22 >/dev/null 2>&1 || true; export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"; }
val_up(){ docker ps --format '{{.Names}}' | grep -cE 'validator[0-9]+_(cl|el|el_pay)$'; }
bn(){ cast block-number --rpc-url "http://127.0.0.1:$1" 2>/dev/null; }
host_ip(){ tailscale ip -4 2>/dev/null | head -1 || hostname -I 2>/dev/null | awk '{print $1}'; }

# Patch the quake-generated genesis into the payment-lane genesis: chainId 1338 + 200M gas
# (header gasLimit AND the ProtocolConfig storage slot).
make_payment_genesis(){
  local src="$REPO/.quake/$SCEN/assets/genesis.json"
  local dst="$REPO/.quake/$SCEN/assets/payment-genesis.json"
  PAY_CHAINID="$PAY_CHAINID" PAY_GAS="$PAY_GAS" PROTO="$PROTO_CFG_ADDR" SLOT="$GAS_SLOT" \
  python3 - "$src" "$dst" <<'PY'
import json, os, sys
src, dst = sys.argv[1], sys.argv[2]
g = json.load(open(src))
g["config"]["chainId"] = int(os.environ["PAY_CHAINID"])
gas = int(os.environ["PAY_GAS"])
g["gasLimit"] = hex(gas)
# ProtocolConfig blockGasLimit slot (Arc reads this at runtime, not just the header)
proto = os.environ["PROTO"].lower()
slot  = os.environ["SLOT"].lower()
acct = None
for k in g["alloc"]:
    if k.lower() == proto:
        acct = g["alloc"][k]; break
assert acct is not None, "ProtocolConfig account not found in genesis alloc"
st = acct.setdefault("storage", {})
# normalise: find the existing slot key regardless of 0x/padding, overwrite it
val = "0x" + f"{gas:064x}"
existing = [kk for kk in st if kk.lower().replace('0x','').lstrip('0').rjust(64,'0') == slot.replace('0x','')]
for kk in existing: del st[kk]
st[slot] = val
json.dump(g, open(dst, "w"))
print(f"payment-genesis.json: chainId={g['config']['chainId']} gasLimit={g['gasLimit']} slot[{slot[:14]}...]={val[:12]}...")
PY
}

start(){
  setup_env
  [ "$(val_up)" -gt 0 ] && { echo "!! validators already running -- stop the other demo first"; exit 1; }

  echo "==> [1/5] EVM lane: 4 validators (CL + EVM-EL) at ${EVM_GAS} gas (30M)..."
  target/release/quake -f "crates/quake/scenarios/${SCEN}.toml" start \
    -e "$EXTRA_ACCOUNTS" --monitoring false --force --block-gas-limit "$EVM_GAS" >"$RUN/quake.log" 2>&1 || true
  [ "$(val_up)" -ge 8 ] || { echo "!! start failed -- see $RUN/quake.log"; tail -5 "$RUN/quake.log"; exit 1; }

  echo "==> [2/5] payment lane genesis: chainId ${PAY_CHAINID}, ${PAY_GAS} gas (200M)..."
  cp assets/localdev/payment-jwt.hex ".quake/${SCEN}/assets/" 2>/dev/null || true
  make_payment_genesis || { echo "!! payment genesis patch failed"; exit 1; }

  echo "==> [3/5] payment ELs (200M gas, chainId ${PAY_CHAINID}), gossip-peered..."
  PAYMENT_GENESIS=payment-genesis.json TESTNET="$SCEN" \
    bash experiments/dual-el/launch-payment-els.sh >"$RUN/paylane.log" 2>&1

  echo "==> [4/5] waiting for both lanes to produce..."
  for i in $(seq 1 40); do e=$(bn 8645); p=$(bn 19645); [ -n "$e" ] && [ -n "$p" ] && [ "$p" -ge 3 ] && { echo "    lanes live (EVM $e / PAY $p)"; break; }; sleep 3; done
  # safety-net caps
  for i in 1 2 3 4; do
    docker update --memory 768m --memory-swap 768m validator${i}_cl >/dev/null 2>&1
    docker update --memory 5g   --memory-swap 5g   validator${i}_el >/dev/null 2>&1
    docker update --memory 6g   --memory-swap 6g   validator${i}_el_pay >/dev/null 2>&1
  done

  echo "==> [4b/5] block time -> ${BLOCK_TIME_MS}ms (Arc's 2 blocks/s product cadence)..."
  bash "$REPO/experiments/dual-el/set-block-time.sh" "$BLOCK_TIME_MS" || echo "  !! block-time update failed (chain keeps genesis 250ms)"

  echo "==> [4c/5] lane economics: payment fee FIXED at ${PAY_FIXED_FEE:-1000000000} wei (minBaseFee==maxBaseFee)..."
  # retry: on a fresh chain the first controller txs can be dropped in the quorum race
  for i in 1 2 3 4 5; do
    EVM_GAS="$EVM_GAS" PAY_GAS="$PAY_GAS" PAY_FIXED_FEE="${PAY_FIXED_FEE:-1000000000}" \
      bash "$REPO/experiments/dual-el/fleet/set-lane-economics.sh" apply && break
    echo "  retry $i/5..."; sleep 5
  done

  echo "==> [5/5] dashboard..."
  old=$(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  nohup python3 "$DASH" >"$RUN/dashboard.log" 2>&1 & echo $! >"$RUN/dash.pid"

  echo; echo "  ✅ two-lane demo up.  dashboard -> http://localhost:$PORT"
  echo; metamask
}

# Verify the gas limits actually took, then print MetaMask setup.
metamask(){
  setup_env
  local ip; ip=$(host_ip)
  local egl pgl ecid pcid
  egl=$(cast rpc --rpc-url http://127.0.0.1:8545 eth_getBlockByNumber latest false 2>/dev/null | python3 -c "import sys,json;print(int(json.load(sys.stdin)['gasLimit'],16))" 2>/dev/null)
  pgl=$(cast rpc --rpc-url http://127.0.0.1:19545 eth_getBlockByNumber latest false 2>/dev/null | python3 -c "import sys,json;print(int(json.load(sys.stdin)['gasLimit'],16))" 2>/dev/null)
  ecid=$(cast chain-id --rpc-url http://127.0.0.1:8545 2>/dev/null)
  pcid=$(cast chain-id --rpc-url http://127.0.0.1:19545 2>/dev/null)
  cat <<EOF
  ================  MetaMask setup  ================

  1) Import the dev account (Add account / Import > Private key):
       0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
     (address 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 -- prefunded 1,000,000 on both lanes)

  2) Add two networks (Settings > Networks > Add network manually):

     EVM lane
       Network name   Arc EVM
       RPC URL        http://${ip:-127.0.0.1}:8545
       Chain ID       ${ecid:-$EVM_CHAINID}
       Currency       ARC
       Block gas limit (verify) ${egl:-?}   (target 30,000,000)

     Payment lane
       Network name   Arc Payments
       RPC URL        http://${ip:-127.0.0.1}:19545
       Chain ID       ${pcid:-$PAY_CHAINID}
       Currency       ARC
       Block gas limit (verify) ${pgl:-?}   (target 200,000,000)

  Switch networks in the dropdown to see the two balances. Sending on one lane does not
  touch the other. If your browser is on another machine, use the host IP ${ip:-<host-ip>}.

  Note: MetaMask may warn that the symbol should be "ETH" -- chain ID 1337 is a well-known dev
  chain in its registry, so it expects ETH. The symbol is only a display label; the warning is
  harmless, just click through. (Enter "ETH" for the EVM lane if you'd rather not see it.)
  =================================================
EOF
}

status(){
  setup_env
  echo "containers: $(val_up)/12"
  echo "  EVM lane  head=$(bn 8545)   chainId=$(cast chain-id --rpc-url http://127.0.0.1:8545 2>/dev/null)"
  echo "  PAY lane  head=$(bn 19545)  chainId=$(cast chain-id --rpc-url http://127.0.0.1:19545 2>/dev/null)"
}

stop(){
  echo "==> stopping..."; [ -f "$RUN/dash.pid" ] && kill "$(cat "$RUN/dash.pid")" 2>/dev/null
  old=$(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+' | head -1); [ -n "$old" ] && kill "$old" 2>/dev/null
  pkill -x spammer 2>/dev/null; sleep 1
  ids=$(docker ps -aq --filter "name=validator"); [ -n "$ids" ] && docker rm -f $ids >/dev/null 2>&1
  for n in arc_testnet_default arc_testnet_host-access arc_testnet_blockscout arc_testnet_monitoring_default; do docker network rm "$n" >/dev/null 2>&1 || true; done
  [ -d ".quake/$SCEN" ] && docker run --rm -v "$REPO/.quake":/q --user root alpine rm -rf "/q/$SCEN" 2>/dev/null
  rm -f "$RUN"/*.pid; echo "  ✅ stopped and cleaned."
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  metamask) metamask ;;
  *) echo "usage: $0 {start|stop|status|metamask}"; exit 1 ;;
esac
