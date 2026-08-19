#!/bin/bash
# Generate a pre-signed raw-tx corpus for fast testnet prefills.
# Signs N txs per account ONCE (chainId 1338, nonce 0.., fixed fee band) via the
# spammer's SPAM_DUMP_FILE mode against a THROWAWAY endpoint (txs are dumped,
# not sent). Produces per-machine files with disjoint account ranges.
#
# Usage: TXS_PER_ACCT=240 ACCTS=200 MACHINES=4 OUT=/tmp/corpus ./gen-corpus.sh <ws-url>
# 4 machines x 200 accts x 240 txs = 192,000 txs (~one 190k fill).
set -e
WS=${1:?ws url of any live payment EL (nonce source only; nothing is sent)}
TXS=${TXS_PER_ACCT:-240}; A=${ACCTS:-200}; M=${MACHINES:-4}; OUT=${OUT:-/tmp/corpus}
SP=$(dirname "$0")/../../target/release/spammer
for m in $(seq 0 $((M-1))); do
  off=$((m*A))
  rm -f ${OUT}-m${m}.*
  SPAM_DUMP_FILE=${OUT}-m${m} "$SP" ws --targets "$WS" -r 100000 -g 1 -a $A \
    --account-offset $off -t 1 --chain-id 1338 -l --max-txs $((A*TXS)) 2>/dev/null || true
  cat ${OUT}-m${m}.* > ${OUT}-m${m}.txt && rm -f ${OUT}-m${m}.[0-9]*
  echo "machine $m: $(wc -l < ${OUT}-m${m}.txt) txs -> ${OUT}-m${m}.txt"
done
