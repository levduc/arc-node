# Multi-machine dual-EL fleet (tailscale) — plan + recon (2026-07-15)

Goal: one validator (CL + EVM-EL + payment-EL) per physical machine to remove the shared-62G-box
RAM ceiling (caps/sheds/OOM cascades; two hard freezes). Bonus: real failure domains (machine-kill
BFT demo), real network latency, 30M+ preseeds become feasible.

## Fleet (all x86_64, docker 29.6.1, docker-group ok; tailscale ssh enabled; LAN 2ms direct)
| host            | tailscale IP     | RAM | cores | disk | role |
|-----------------|------------------|-----|-------|------|------|
| ginny-alienware | 100.124.148.61   | 62G | 16    | -    | validator1 + quake orchestration, dashboard, spammers |
| ginnythui       | 100.85.150.119   | 62G | 16    | 470G | validator2 |
| papaduck        | 100.70.62.92     | 78G | 12    | 146G | validator3 (most RAM) |
| papaduck-alien2 | 100.86.97.40     | 15G | 20    | 328G | validator4 (10M-config only; tight for 30M) |

## Topology mechanics (all decoded from .quake/soak4/compose.yaml)
- CL P2P: `--p2p.persistent-peers=/ip4/IP/tcp/PORT,...`; host ports 27000..27003 -> container 27000.
  Cross-machine: valN entry = /ip4/<its host tailscale IP>/tcp/2700(N-1). Same string on every node
  (self-entries are tolerated / hairpin via tailscale local IP works).
- EVM EL P2P: `--trusted-peers=enode://KEY@IP:30303`; NOT host-published today -> add "3030N:30303"
  per EL and rewrite enode IPs to tailscale IPs. enode KEY comes from the EL's fixed p2p key
  (assets/entrypoint or datadir discovery-secret; verify it is stable across restart).
- Payment EL P2P: ours (launch-payment-els.sh): publish 30303 (e.g. 3031N) and admin_addPeer with
  enode@<tailscale IP>:<published port> instead of container-net IPs.
- CL <-> ELs: IPC sockets, machine-local, unchanged. Engine endpoints unchanged.
- Identity/data: per-validator tree .quake/soak4/validatorN (malachite keys/store, reth datadirs)
  + shared assets/ (genesis, payment-genesis, jwt). rsync to each machine, same paths under $HOME.
- Images: arc_execution + arc_consensus docker-saved/loaded to every box (script /tmp/shipimages.sh).

## Execution phases
1. [DONE] recon + ssh; [RUNNING] image ship.
2. PILOT (2 machines): generate testnet here (quake start then stop containers), rewrite peers
   (CLs + EVM enodes) to tailscale scheme, extract validator4's services into a standalone
   compose/launch script, rsync tree to papaduck, start val1-3 here + val4 there; verify block
   production + both-lane agreement; then kill/restart val4 remotely to validate ops.
3. FULL SPREAD: one validator per box; 10M preseed (init once here, rsync datadir); spammers target
   all four via tailscale IPs; dashboard endpoints per-host IPs; run soak.sh (demo-bloat profile).
4. DEMO: machine-kill fault tolerance; optional 30M preseed (skip alien2 or give it the CL-only role).

## Gotchas to carry over
- Boot pay ELs uncapped then clamp (genesis parse transient) — per machine now trivial (no caps
  needed at all except alien2).
- Spammer nonce rule: any EL restart -> respawn spammers with -l (soak.sh loops already do).
- reth genesis re-parse at every boot: keep per-machine boots sequential-ish on alien2 (15G).
- tailscale MTU/UDP: fine on LAN (2ms direct); if a machine falls back to DERP relay, expect
  latency jumps — check `tailscale status` shows "direct" for all pairs before blaming consensus.
