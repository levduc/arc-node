#!/usr/bin/env bash
# Set per-lane gas economics ON A RUNNING testnet via the on-chain ProtocolConfig
# (proxy 0x3600...0001) using the controller key quake generated. Zero node code,
# zero restarts, consensus-safe by construction (every validator reads the same
# contract state; the executor re-reads FeeParams every block).
#
#   EVM lane     -> blockGasLimit = 30M AND Arc MAINNET's real fee band (min 20 gwei / max
#                   20,000 gwei, from assets/mainnet/genesis.json) — an idle devnet otherwise
#                   decays to the 1-wei dev floor and reads absurdly cheap (<$0.000001/transfer,
#                   cheaper than the payment lane, inverting the demo story)
#   payment lane -> blockGasLimit = 100M  AND minBaseFee == maxBaseFee  (FIXED gas price)
#
#   ./set-lane-economics.sh apply    (default)
#   ./set-lane-economics.sh show     (read feeParams from both lanes)
# Env: EVM_GAS=30000000 PAY_GAS=100000000 PAY_FIXED_FEE=1000000000 (wei; default 1 gwei)
#      EVM_MIN_FEE=20000000000 EVM_MAX_FEE=20000000000000 (wei; Arc mainnet's band)
#      EVM_RPC=http://127.0.0.1:8545 PAY_RPC=http://127.0.0.1:19545
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"; cd "$REPO"
export PATH="$HOME/.foundry/bin:$PATH"
PC=0x3600000000000000000000000000000000000001
EVM_RPC=${EVM_RPC:-http://127.0.0.1:8545}
PAY_RPC=${PAY_RPC:-http://127.0.0.1:19545}
EVM_GAS=${EVM_GAS:-30000000}
PAY_GAS=${PAY_GAS:-100000000}
PAY_FIXED_FEE=${PAY_FIXED_FEE:-1000000000}
EVM_MIN_FEE=${EVM_MIN_FEE:-20000000000}      # 20 gwei    = Arc mainnet minBaseFee ($0.00042/transfer)
EVM_MAX_FEE=${EVM_MAX_FEE:-20000000000000}   # 20,000 gwei = Arc mainnet maxBaseFee ($0.42/transfer)
CFG=".quake/soak4/assets/controllers-config.json"
SIG_FP="feeParams()((uint64,uint64,uint64,uint256,uint256,uint256))"

controller_key(){
  # ProtocolConfig's controller is the genesis provisioner: resolve controller() on-chain,
  # then derive the matching key from the standard dev mnemonic (fallback: controllers-config).
  local want; want=$(cast call $PC "controller()(address)" --rpc-url "$EVM_RPC" 2>/dev/null)
  local MN="test test test test test test test test test test test junk"
  for i in $(seq 0 20); do
    if [ "$(cast wallet address --mnemonic "$MN" --mnemonic-index $i 2>/dev/null)" = "$want" ]; then
      cast wallet private-key --mnemonic "$MN" --mnemonic-index $i; return
    fi
  done
  python3 - "$CFG" "$want" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); want=sys.argv[2].lower()
for v in d.values():
    if isinstance(v,dict) and v.get("address","").lower()==want:
        print(v.get("signingKey","")); break
PY
}

show(){
  for spec in "EVM $EVM_RPC" "PAY $PAY_RPC"; do
    lane=${spec%% *}; rpc=${spec##* }
    echo "== $lane lane ($rpc) =="
    cast call $PC "$SIG_FP" --rpc-url "$rpc" 2>/dev/null \
      | sed 's/[()]//g' | tr ',' '\n' | paste -d: <(printf "alpha\nkRate\ninvElasticity\nminBaseFee\nmaxBaseFee\nblockGasLimit\n") - 2>/dev/null || echo "  (call failed)"
    b=$(cast block latest --rpc-url "$rpc" --json 2>/dev/null | python3 -c "import sys,json;b=json.load(sys.stdin);print('block gasLimit',int(b['gasLimit'],16),'baseFee',int(b.get('baseFeePerGas','0x0'),16))" 2>/dev/null)
    echo "  observed: $b"
  done
}

apply(){
  KEY=$(controller_key)
  [ -n "$KEY" ] || { echo "!! no controller key found in $CFG"; exit 1; }
  ADDR=$(cast wallet address "$KEY")
  echo "controller: $ADDR"

  echo "==> EVM lane: gas $EVM_GAS + Arc mainnet fee band (min $EVM_MIN_FEE / max $EVM_MAX_FEE wei)"
  ecur=$(cast call $PC "$SIG_FP" --rpc-url "$EVM_RPC" | sed 's/[()]//g')
  ealpha=$(echo "$ecur" | cut -d, -f1 | tr -d " "); ekrate=$(echo "$ecur" | cut -d, -f2 | tr -d " "); einv=$(echo "$ecur" | cut -d, -f3 | tr -d " ")
  echo "    keeping alpha=$ealpha kRate=$ekrate invElasticity=$einv"
  cast send $PC "updateFeeParams((uint64,uint64,uint64,uint256,uint256,uint256))" \
    "($ealpha,$ekrate,$einv,$EVM_MIN_FEE,$EVM_MAX_FEE,$EVM_GAS)" \
    --private-key "$KEY" --rpc-url "$EVM_RPC" --timeout 60 >/dev/null && echo "    sent"

  echo "==> payment lane: read current FeeParams, pin minBaseFee=maxBaseFee=$PAY_FIXED_FEE wei, gas $PAY_GAS"
  cur=$(cast call $PC "$SIG_FP" --rpc-url "$PAY_RPC" | sed 's/[()]//g')
  alpha=$(echo "$cur" | cut -d, -f1 | tr -d " "); krate=$(echo "$cur" | cut -d, -f2 | tr -d " "); inv=$(echo "$cur" | cut -d, -f3 | tr -d " ")
  echo "    keeping alpha=$alpha kRate=$krate invElasticity=$inv"
  cast send $PC "updateFeeParams((uint64,uint64,uint64,uint256,uint256,uint256))" \
    "($alpha,$krate,$inv,$PAY_FIXED_FEE,$PAY_FIXED_FEE,$PAY_GAS)" \
    --private-key "$KEY" --rpc-url "$PAY_RPC" --timeout 60 >/dev/null && echo "    sent"

  echo "==> waiting 3 blocks for effect…"
  sleep 8
  show
  # verify-or-fail: report nonzero unless BOTH lanes show the requested limits
  eg=$(cast block latest --rpc-url "$EVM_RPC" --json 2>/dev/null | python3 -c "import sys,json;print(int(json.load(sys.stdin)['gasLimit'],16))" 2>/dev/null)
  pg=$(cast block latest --rpc-url "$PAY_RPC" --json 2>/dev/null | python3 -c "import sys,json;print(int(json.load(sys.stdin)['gasLimit'],16))" 2>/dev/null)
  if [ "$eg" = "$EVM_GAS" ] && [ "$pg" = "$PAY_GAS" ]; then
    echo "✅ economics VERIFIED (EVM $eg / PAY $pg)"
  else
    echo "❌ economics NOT applied yet (EVM ${eg:-?} vs $EVM_GAS, PAY ${pg:-?} vs $PAY_GAS)"; return 1
  fi
  echo "NOTE: Arc derives each block's gas limit directly from ProtocolConfig"
  echo "      (expected_gas_limit), so the new limit applies from the NEXT block."
}

case "${1:-apply}" in apply) apply;; show) show;; *) echo "usage: $0 [apply|show]"; exit 1;; esac
