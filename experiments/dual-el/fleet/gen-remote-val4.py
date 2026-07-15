#!/usr/bin/env python3
"""Fleet pilot: rewrite the quake compose for a val4-on-remote topology.

Reads .quake/soak4/compose.yaml and produces:
  1. compose.yaml PATCHED IN PLACE (backup .prefleet) for the LOCAL host:
     - val1-3 CL persistent-peers: val4's entry (172.21.1.3) -> /ip4/<REMOTE_TS>/tcp/27003
     - val1-3 EVM EL trusted-peers: val4's enode IP (172.21.2.3:30303) -> <REMOTE_TS>:30403
     - val1-3 EVM ELs get published P2P ports 3030N:30303 (so remote val4 can dial them)
  2. .quake/soak4/compose-val4.yaml for the REMOTE host:
     - only validator4_cl + validator4_el services (+ fresh networks)
     - volume host paths  LOCAL_BASE -> REMOTE_BASE
     - val4 CL persistent-peers -> /ip4/<LOCAL_TS>/tcp/2700{0,1,2}
     - val4 EL trusted-peers    -> enode KEYs @ <LOCAL_TS>:3030{1,2,3}
     - val4 EL publishes 30403:30303
Peer enode KEYS are preserved verbatim; only IP:port endpoints are rewritten.
"""
import re, sys, yaml, copy

LOCAL_TS = "100.124.148.61"   # ginny-alienware
REMOTE_TS = "100.70.62.92"    # papaduck
LOCAL_BASE = "/home/papaduck/arc-node-paymentlane/.quake/soak4"
REMOTE_BASE = "/home/papaduck/arc-fleet/soak4"
COMPOSE = f"{LOCAL_BASE}/compose.yaml"

# CL container IPs 172.21.1.<N-1>; EL container IPs 172.21.2.<N-1> (quake subnet scheme)
CL_IP = lambda n: f"172.21.1.{n-1}"
EL_IP = lambda n: f"172.21.2.{n-1}"
CL_HOSTPORT = lambda n: 27000 + (n - 1)          # host-published CL P2P
EL_HOSTPORT = lambda n: 30300 + n                # NEW: host-published EVM EL P2P (30301..30304)

def rewrite_cmd(cmd, subs):
    out = []
    for arg in cmd:
        for old, new in subs:
            arg = arg.replace(old, new)
        out.append(arg)
    return out

c = yaml.safe_load(open(COMPOSE))
orig = copy.deepcopy(c)

# ---- local patch: point val1-3 at remote val4, publish their EL p2p ----
for n in (1, 2, 3):
    cl = c["services"][f"validator{n}_cl"]
    cl["command"] = rewrite_cmd(cl["command"], [
        (f"/ip4/{CL_IP(4)}/tcp/27000", f"/ip4/{REMOTE_TS}/tcp/{CL_HOSTPORT(4)}"),
    ])
    el = c["services"][f"validator{n}_el"]
    el["command"] = rewrite_cmd(el["command"], [
        (f"@{EL_IP(4)}:30303", f"@{REMOTE_TS}:30403"),
    ])
    ports = el.setdefault("ports", [])
    pub = f"{EL_HOSTPORT(n)}:30303"
    if pub not in ports:
        ports.append(pub)

# drop val4 services locally (they move to the remote host)
remote_services = {}
for name in ("validator4_cl", "validator4_el"):
    remote_services[name] = c["services"].pop(name)

# ---- remote compose: val4 only, paths + peers rewritten ----
for name, svc in remote_services.items():
    svc["volumes"] = [v.replace(LOCAL_BASE, REMOTE_BASE) for v in svc["volumes"]]
    svc.pop("depends_on", None)
subs_cl = [(f"/ip4/{CL_IP(n)}/tcp/27000", f"/ip4/{LOCAL_TS}/tcp/{CL_HOSTPORT(n)}") for n in (1, 2, 3)]
remote_services["validator4_cl"]["command"] = rewrite_cmd(remote_services["validator4_cl"]["command"], subs_cl)
subs_el = [(f"@{EL_IP(n)}:30303", f"@{LOCAL_TS}:{EL_HOSTPORT(n)}") for n in (1, 2, 3)]
el4 = remote_services["validator4_el"]
el4["command"] = rewrite_cmd(el4["command"], subs_el)
el4.setdefault("ports", []).append("30403:30303")

remote = {
    "services": remote_services,
    "networks": {"default": {"name": "arc_testnet_default"},
                 "host-access": {"name": "arc_testnet_host-access"}},
}

open(COMPOSE + ".prefleet", "w").write(yaml.dump(orig, sort_keys=False))
open(COMPOSE, "w").write(yaml.dump(c, sort_keys=False))
open(f"{LOCAL_BASE}/compose-val4.yaml", "w").write(yaml.dump(remote, sort_keys=False))
print("wrote: patched compose.yaml (+.prefleet backup) and compose-val4.yaml")
# sanity: show the rewritten peer args
for svc, key in (("validator1_cl", "persistent-peers"), ("validator1_el", "trusted-peers")):
    for a in c["services"][svc]["command"]:
        if key in str(a): print(f"{svc}: {str(a)[:160]}")
for svc, key in (("validator4_cl", "persistent-peers"), ("validator4_el", "trusted-peers")):
    for a in remote_services[svc]["command"]:
        if key in str(a): print(f"{svc}(remote): {str(a)[:160]}")
