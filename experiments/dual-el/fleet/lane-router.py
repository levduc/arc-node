#!/usr/bin/env python3
"""Lane-multiplex RPC router: one endpoint in front of two execution lanes.

Reads the (tamper-proof, signature-covered) chainId out of each raw transaction and forwards
eth_sendRawTransaction to the lane whose chainId matches. Everything else is served by a default
lane; per-lane reads are addressed by path (/evm, /pay).

  EVM lane  chainId 1337 -> EVM_RPC   (default)
  Payment   chainId 1338 -> PAY_RPC

  ./lane-router.py                       # listens :9000, routes by tx chainId
  env: ROUTER_PORT EVM_RPC PAY_RPC EVM_CHAINID PAY_CHAINID
Query a specific lane directly:  POST /evm  or  POST /pay  (bypasses routing).

This is a demo prototype: it unifies the WRITE path (send a signed tx to one URL, it lands on the
right lane). Reads carry no chainId, so balance/receipt queries still need a lane-addressed path —
full read unification requires an aggregating RPC, out of scope here.
"""
import os, json, http.server, urllib.request, rlp_min

EVM_RPC = os.environ.get("EVM_RPC", "http://127.0.0.1:8545")
PAY_RPC = os.environ.get("PAY_RPC", "http://127.0.0.1:19545")
EVM_CHAINID = int(os.environ.get("EVM_CHAINID", "1337"))
PAY_CHAINID = int(os.environ.get("PAY_CHAINID", "1338"))
PORT = int(os.environ.get("ROUTER_PORT", "9000"))
LANE = {EVM_CHAINID: EVM_RPC, PAY_CHAINID: PAY_RPC}

def forward(url, body):
    r = urllib.request.Request(url, data=body, headers={"content-type": "application/json"})
    return urllib.request.urlopen(r, timeout=20).read()

def route_raw_tx(raw_hex):
    """Return the target RPC url by decoding the tx's chainId, or None if undecodable."""
    try:
        cid = rlp_min.tx_chain_id(bytes.fromhex(raw_hex[2:] if raw_hex.startswith("0x") else raw_hex))
        return LANE.get(cid), cid
    except Exception:
        return None, None

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, data, code=200):
        self.send_response(code); self.send_header("content-type", "application/json")
        self.end_headers(); self.wfile.write(data)
    def do_POST(self):
        n = int(self.headers.get("content-length", 0)); body = self.rfile.read(n)
        # explicit lane paths bypass routing
        if self.path == "/evm": return self._send(forward(EVM_RPC, body))
        if self.path == "/pay": return self._send(forward(PAY_RPC, body))
        try: req = json.loads(body)
        except Exception: return self._send(b'{"error":"bad json"}', 400)
        method = req.get("method")
        if method == "eth_sendRawTransaction":
            raw = req["params"][0]
            url, cid = route_raw_tx(raw)
            if url is None:
                return self._send(json.dumps({"jsonrpc":"2.0","id":req.get("id"),
                    "error":{"code":-32000,"message":f"no lane for chainId {cid}"}}).encode())
            print(f"  route tx chainId={cid} -> {url}")
            return self._send(forward(url, body))
        # all other methods: default to EVM lane (reads should use /evm or /pay for the other)
        return self._send(forward(EVM_RPC, body))

if __name__ == "__main__":
    print(f"lane-router :{PORT}  |  chainId {EVM_CHAINID}->EVM({EVM_RPC})  {PAY_CHAINID}->PAY({PAY_RPC})")
    http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
