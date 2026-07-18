#!/usr/bin/env python3
"""Minimal mock CL: drive one EL over the Engine API, producing a block every interval from its
mempool. Args: <http_port> <auth_port> <jwt_path> <block_ms>."""
import json, urllib.request, hmac, hashlib, base64, time, sys

HTTP, AUTH, JWT, BLOCK_MS = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
secret = bytes.fromhex(open(JWT).read().strip())

def b64u(b): return base64.urlsafe_b64encode(b).rstrip(b'=')
def jwt():
    h = b64u(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    p = b64u(json.dumps({"iat": int(time.time())}).encode())
    return (h + b'.' + p + b'.' + b64u(hmac.new(secret, h + b'.' + p, hashlib.sha256).digest())).decode()

def rpc(port, m, params, auth=True, tmo=30):
    r = urllib.request.Request(
        f'http://127.0.0.1:{port}',
        data=json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': m, 'params': params}).encode(),
        headers={'content-type': 'application/json', 'Authorization': 'Bearer ' + jwt()})
    o = json.load(urllib.request.urlopen(r, timeout=tmo))
    if 'error' in o:
        raise RuntimeError(o['error'])
    return o['result']

# start from genesis
gen = rpc(HTTP, 'eth_getBlockByNumber', ['0x0', False], auth=False)
head = gen['hash']
fee = "0x" + "11" * 20
while True:
    t0 = time.time()
    try:
        attrs = {"timestamp": hex(int(time.time())), "prevRandao": "0x" + "00" * 32,
                 "suggestedFeeRecipient": fee, "withdrawals": [], "parentBeaconBlockRoot": head}
        r = rpc(AUTH, 'engine_forkchoiceUpdatedV3',
                [{"headBlockHash": head, "safeBlockHash": head, "finalizedBlockHash": head}, attrs])
        pid = r['payloadId']
        time.sleep(max(0.05, BLOCK_MS / 1000.0))  # let the builder pack
        p = rpc(AUTH, 'engine_getPayloadV4', [pid])
        ep = p['executionPayload']
        rpc(AUTH, 'engine_newPayloadV4', [ep, [], head, p.get('executionRequests', [])])
        rpc(AUTH, 'engine_forkchoiceUpdatedV3',
            [{"headBlockHash": ep['blockHash'], "safeBlockHash": ep['blockHash'],
              "finalizedBlockHash": ep['blockHash']}, None])
        head = ep['blockHash']
    except Exception as e:
        # transient (e.g. RPC busy) — brief backoff, keep the chain going
        time.sleep(0.2)
    dt = time.time() - t0
    if dt < BLOCK_MS / 1000.0:
        time.sleep(BLOCK_MS / 1000.0 - dt)
