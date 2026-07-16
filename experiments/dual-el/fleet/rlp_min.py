"""Minimal chainId extractor for Ethereum raw transactions (no deps).
Handles EIP-1559 (0x02), EIP-2930 (0x01), EIP-4844 (0x03) — chainId is the FIRST list element —
and legacy/EIP-155 (derives chainId from v). Only decodes what's needed to read chainId."""

def _decode_len(b, i):
    """Return (payload_start, payload_len, next_index) for the RLP item at b[i]."""
    p = b[i]
    if p < 0x80:      return i, 1, i + 1                       # single byte
    if p < 0xb8:      return i + 1, p - 0x80, i + 1 + (p - 0x80)   # short string
    if p < 0xc0:
        ll = p - 0xb7; n = int.from_bytes(b[i+1:i+1+ll], "big"); return i+1+ll, n, i+1+ll+n
    if p < 0xf8:      return i + 1, p - 0xc0, i + 1 + (p - 0xc0)   # short list
    ll = p - 0xf7; n = int.from_bytes(b[i+1:i+1+ll], "big"); return i+1+ll, n, i+1+ll+n

def _read_int(b, i):
    s, ln, nxt = _decode_len(b, i)
    return int.from_bytes(b[s:s+ln], "big"), nxt

def tx_chain_id(raw: bytes) -> int:
    t = raw[0]
    if t in (0x01, 0x02, 0x03):          # typed: rlp list, chainId is first element
        s, ln, _ = _decode_len(raw, 1)   # open the list after the type byte
        cid, _ = _read_int(raw, s)
        return cid
    # legacy: rlp([nonce, gasPrice, gas, to, value, data, v, r, s]); chainId = (v-35)//2
    s, ln, _ = _decode_len(raw, 0)
    i = s
    for _ in range(6):                    # skip nonce..data
        _, i = _read_int(raw, i)
    v, _ = _read_int(raw, i)
    if v in (27, 28):                     # pre-EIP155, no chainId
        return 0
    return (v - 35) // 2
