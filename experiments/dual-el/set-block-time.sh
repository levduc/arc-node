#!/usr/bin/env bash
# Set the chain's target block time AT RUNTIME via the ProtocolConfig contract -- the real
# governance path, no genesis change, no restart.
#
#   ./set-block-time.sh 500        # 2 blocks/s (Arc's product target)
#   ./set-block-time.sh 250        # localdev genesis default (4 blocks/s)
#   ./set-block-time.sh            # show current value + measured cadence
#
# How it works: every height, the CL fetches consensusParams() from the ProtocolConfig contract
# (0x3600...0001) on the PRIMARY (EVM) lane and paces the next height by targetBlockTimeMs
# (malachite "stable block times"; 0 disables). localdev genesis ships 250ms; Arc's product
# target is 500ms. updateConsensusParams is onlyController -- in localdev the controller is
# hardhat dev account #8, so we can flip it live with one transaction. Takes effect ~2 blocks
# after inclusion (the CL reads params at each decided height for the next one).
set -uo pipefail
export PATH="$HOME/.foundry/bin:$PATH"
RPC="${RPC:-http://127.0.0.1:8545}"          # EVM lane (the CL paces off the PRIMARY engine)
PROTO=0x3600000000000000000000000000000000000001
SIG_READ='consensusParams()((uint16,uint16,uint16,uint16,uint16,uint16,uint16,uint16))'
SIG_WRITE='updateConsensusParams((uint16,uint16,uint16,uint16,uint16,uint16,uint16,uint16))'
# localdev ProtocolConfig controller = hardhat dev account #8 (m/44'/60'/0'/0/8 of the junk mnemonic)
CTRL_KEY="${CTRL_KEY:-0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97}"

cur(){ cast call --rpc-url "$RPC" "$PROTO" "$SIG_READ" 2>/dev/null | tr -d '()' ; }

measure(){ # observed cadence over ~6s
  local h1 h2
  h1=$(cast block-number --rpc-url "$RPC" 2>/dev/null) || return
  sleep 6
  h2=$(cast block-number --rpc-url "$RPC" 2>/dev/null) || return
  python3 -c "b=$h2-$h1; print(f'  measured: {b/6:.2f} blocks/s ({6000/b if b else 0:.0f} ms/block) over 6s')"
}

vals=$(cur)
[ -n "$vals" ] || { echo "!! EVM lane not reachable at $RPC"; exit 1; }
IFS=', ' read -r p pd pv pvd pc pcd rb tgt <<<"$(echo "$vals" | tr ',' ' ')"
echo "current: targetBlockTimeMs=$tgt  (timeouts: propose=$p/$pd prevote=$pv/$pvd precommit=$pc/$pcd rebroadcast=$rb)"

if [ -z "${1:-}" ]; then measure; exit 0; fi

MS=$1
[ "$MS" -ge 0 ] 2>/dev/null || { echo "usage: $0 [target-block-time-ms]"; exit 1; }
if [ "$tgt" = "$MS" ]; then echo "already $MS ms -- nothing to do"; measure; exit 0; fi

# controller check (fail loud if this chain's controller isn't the dev account)
addr=$(cast wallet address --private-key "$CTRL_KEY")
echo "-> updateConsensusParams(...targetBlockTimeMs=$MS) from controller $addr"
cast send --rpc-url "$RPC" --private-key "$CTRL_KEY" "$PROTO" "$SIG_WRITE" \
  "($p,$pd,$pv,$pvd,$pc,$pcd,$rb,$MS)" --json 2>&1 | python3 -c "
import sys,json
try:
    r=json.load(sys.stdin)
    ok = r.get('status') in ('0x1', 1, '1')
    print(f'  tx {r.get(\"transactionHash\",\"?\")[:18]}...  status={\"success\" if ok else \"REVERTED\"}')
    sys.exit(0 if ok else 1)
except Exception:
    print('  !! send failed (is this account the controller on this chain?)'); sys.exit(1)
" || exit 1

new=$(cur | awk -F', ' '{print $8}')
echo "on-chain now: targetBlockTimeMs=$new"
echo "pacing takes effect within ~2 heights; measuring..."
sleep 4
measure
