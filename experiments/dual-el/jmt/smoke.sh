#!/usr/bin/env bash
# Boot arc-node-execution with a mode, build one empty block via Engine API, print stateRoot.
MODE=$1  # "mpt" or "jmt"
BIN=/home/papaduck/arc-node-paymentlane/target/debug/arc-node-execution
DD=/tmp/jmt-smoke/data-$MODE; rm -rf "$DD"
ENVJMT=""; [ "$MODE" = jmt ] && ENVJMT="ARC_PAYMENT_ROOT=jmt ARC_JMT_STORE_PATH=/tmp/jmt-smoke/store-$MODE"
rm -rf /tmp/jmt-smoke/store-$MODE
$BIN init --datadir "$DD" --chain /tmp/jmt-smoke/genesis.json >/tmp/jmt-smoke/init-$MODE.log 2>&1
env $ENVJMT $BIN node --datadir "$DD" --chain /tmp/jmt-smoke/genesis.json \
  --http --http.addr 127.0.0.1 --http.port 7545 --http.api eth,net,web3,debug \
  --authrpc.addr 127.0.0.1 --authrpc.port 7551 --authrpc.jwtsecret /tmp/jmt-smoke/jwt.hex \
  --engine.state-root-fallback --engine.disable-parallel-sparse-trie \
  --disable-discovery --ipcdisable --port 30399 --metrics 127.0.0.1:7001 \
  >/tmp/jmt-smoke/node-$MODE.log 2>&1 &
echo $! > /tmp/jmt-smoke/pid-$MODE
sleep 25
python3 - "$MODE" <<'PY'
import json,urllib.request,hmac,hashlib,base64,time,sys
mode=sys.argv[1]
secret=bytes.fromhex(open('/tmp/jmt-smoke/jwt.hex').read().strip())
def b64u(b): return base64.urlsafe_b64encode(b).rstrip(b'=')
def jwt():
    h=b64u(json.dumps({"alg":"HS256","typ":"JWT"}).encode()); p=b64u(json.dumps({"iat":int(time.time())}).encode())
    return (h+b'.'+p+b'.'+b64u(hmac.new(secret,h+b'.'+p,hashlib.sha256).digest())).decode()
def rpc(port,m,params,auth=True,tmo=30):
    r=urllib.request.Request(f'http://127.0.0.1:{port}',data=json.dumps({'jsonrpc':'2.0','id':1,'method':m,'params':params}).encode(),headers={'content-type':'application/json','Authorization':'Bearer '+jwt()})
    o=json.load(urllib.request.urlopen(r,timeout=tmo));
    if 'error' in o: raise RuntimeError(o['error'])
    return o
gen=rpc(7545,'eth_getBlockByNumber',['0x0',False],auth=False)['result']; gh=gen['hash']; groot=gen['stateRoot']
attrs={"timestamp":hex(int(time.time())),"prevRandao":"0x"+"00"*32,"suggestedFeeRecipient":"0x"+"11"*20,"withdrawals":[],"parentBeaconBlockRoot":gh}
r=rpc(7551,'engine_forkchoiceUpdatedV3',[{"headBlockHash":gh,"safeBlockHash":gh,"finalizedBlockHash":gh},attrs])
pid=r['result']['payloadId']; time.sleep(1)
p=rpc(7551,'engine_getPayloadV4',[pid])['result']; ep=p['executionPayload']
newroot=ep['stateRoot']
r2=rpc(7551,'engine_newPayloadV4',[ep,[],gh,p.get('executionRequests',[])])
print(f"MODE={mode} genesisRoot={groot[:18]} emptyBlockRoot={newroot[:18]} validate={r2['result']['status']}")
open(f'/tmp/jmt-smoke/result-{mode}.txt','w').write(f"{groot} {newroot} {r2['result']['status']}")
PY
kill $(cat /tmp/jmt-smoke/pid-$MODE) 2>/dev/null
