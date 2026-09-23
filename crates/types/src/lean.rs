// Copyright 2026 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Lean payment lane: the block format consensus carries for the lean lane,
//! and the wire encoding of consensus values when the lane is enabled.
//!
//! Lean block bytes (the lean node's canonical form):
//!
//! ```text
//! [parent 32][number u64 LE][timestamp_ms u64 LE][n_txs u32 LE] ([len u32 LE][tx])*
//! commitment = keccak(parent ‖ number LE ‖ timestamp_ms LE ‖ keccak(bytes[48..]))
//! ```
//!
//! The commitment is always recomputed from the bytes and never taken from the
//! wire or from a lean node's answer.

use alloy_rpc_types_engine::ExecutionPayloadV3;
use sha3::{Digest, Keccak256};
use ssz::{Decode, Encode};

use crate::BlockHash;

/// Flag in the length prefix of a lane frame marking a trailing lean block.
pub const LEAN_LANE_BIT: u64 = 1 << 62;

/// Upper bound on transactions in one lean block (decode-time DoS guard).
pub const MAX_LEAN_TXS: usize = 200_000;

/// Bits of the length prefix that hold the EVM payload length (well under 2^40).
const LANE_LEN_MASK: u64 = (1 << 40) - 1;

/// Size of the fixed lean block header.
const LEAN_HEADER_LEN: usize = 32 + 8 + 8 + 4;

/// Decoded header of a lean block, with the commitment recomputed from the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeanBlockRef {
    pub parent: BlockHash,
    pub number: u64,
    pub timestamp_ms: u64,
    pub tx_count: u32,
    pub commitment: BlockHash,
}

/// Lean block bytes together with their decoded view.
///
/// Construction always decodes strictly and recomputes the commitment, so a
/// value of this type means the bytes are well-formed. It says nothing about
/// whether the block links onto the local lean chain: validation is structural
/// only (see `lean_lane` in the consensus crate), and the lean node executes
/// the block once, when it is decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeanLanePayload {
    pub decoded: LeanBlockRef,
    pub bytes: Vec<u8>,
}

impl LeanLanePayload {
    pub fn new(bytes: Vec<u8>) -> eyre::Result<Self> {
        let decoded = decode_lean_block(&bytes)?;
        Ok(Self { decoded, bytes })
    }

    pub fn commitment(&self) -> BlockHash {
        self.decoded.commitment
    }
}

fn le_u64_at(bytes: &[u8], off: usize) -> eyre::Result<u64> {
    let chunk = bytes
        .get(off..)
        .and_then(<[u8]>::first_chunk::<8>)
        .ok_or_else(|| eyre::eyre!("lean block truncated at offset {off} (u64)"))?;
    Ok(u64::from_le_bytes(*chunk))
}

fn le_u32_at(bytes: &[u8], off: usize) -> eyre::Result<u32> {
    let chunk = bytes
        .get(off..)
        .and_then(<[u8]>::first_chunk::<4>)
        .ok_or_else(|| eyre::eyre!("lean block truncated at offset {off} (u32)"))?;
    Ok(u32::from_le_bytes(*chunk))
}

/// Strictly decodes lean block bytes and recomputes their commitment.
///
/// Rejects truncation, trailing bytes and more than [`MAX_LEAN_TXS`] transactions.
pub fn decode_lean_block(bytes: &[u8]) -> eyre::Result<LeanBlockRef> {
    if bytes.len() < LEAN_HEADER_LEN {
        return Err(eyre::eyre!("lean block bytes too short ({})", bytes.len()));
    }
    let parent = BlockHash::from_slice(&bytes[..32]);
    let number = le_u64_at(bytes, 32)?;
    let timestamp_ms = le_u64_at(bytes, 40)?;
    let tx_count = le_u32_at(bytes, 48)?;
    if tx_count as usize > MAX_LEAN_TXS {
        return Err(eyre::eyre!(
            "lean block carries {tx_count} txs (cap {MAX_LEAN_TXS})"
        ));
    }
    let mut off = LEAN_HEADER_LEN;
    for i in 0..tx_count {
        let len = le_u32_at(bytes, off)
            .map_err(|_| eyre::eyre!("lean block truncated at tx {i} length"))?
            as usize;
        off = off
            .checked_add(4)
            .and_then(|o| o.checked_add(len))
            .filter(|&end| end <= bytes.len())
            .ok_or_else(|| eyre::eyre!("lean block truncated at tx {i} body (len {len})"))?;
    }
    if off != bytes.len() {
        return Err(eyre::eyre!(
            "lean block has {} trailing bytes after {tx_count} txs",
            bytes.len().saturating_sub(off)
        ));
    }
    // The tx hash covers the whole framed section (count, lengths and bodies),
    // so two framings of the same body bytes never share a commitment.
    let txs_hash = Keccak256::digest(&bytes[48..]);
    let mut hasher = Keccak256::new();
    hasher.update(parent.as_slice());
    hasher.update(number.to_le_bytes());
    hasher.update(timestamp_ms.to_le_bytes());
    hasher.update(txs_hash);
    Ok(LeanBlockRef {
        parent,
        number,
        timestamp_ms,
        tx_count,
        commitment: BlockHash::from_slice(&hasher.finalize()),
    })
}

/// Encodes a consensus value (proposal data, value-sync bytes) for the wire.
///
/// With the lane off this is the EVM payload's SSZ bytes, exactly as upstream.
/// With the lane on every value is framed, EVM-only or not:
/// `[u64 LE: len(evm SSZ) | LEAN_LANE_BIT?][evm SSZ][lean block bytes?]`.
/// A lean payload is framed regardless of the flag, so it is never dropped.
pub fn encode_value(
    execution_payload: &ExecutionPayloadV3,
    lean: Option<&LeanLanePayload>,
    lean_lane: bool,
) -> Vec<u8> {
    let evm = execution_payload.as_ssz_bytes();
    if !lean_lane && lean.is_none() {
        return evm;
    }
    let lean_bytes = lean.map_or(&[][..], |l| l.bytes.as_slice());
    let mut prefix = evm.len() as u64;
    if lean.is_some() {
        prefix |= LEAN_LANE_BIT;
    }
    let mut buf = Vec::with_capacity(
        8usize
            .saturating_add(evm.len())
            .saturating_add(lean_bytes.len()),
    );
    buf.extend_from_slice(&prefix.to_le_bytes());
    buf.extend_from_slice(&evm);
    buf.extend_from_slice(lean_bytes);
    buf
}

/// Decodes a consensus value; the inverse of [`encode_value`].
///
/// The format is chosen by the node's own flag, never sniffed from the bytes,
/// so a fleet with mixed flags fails closed with decode errors.
pub fn decode_value(
    bytes: &[u8],
    lean_lane: bool,
) -> eyre::Result<(ExecutionPayloadV3, Option<LeanLanePayload>)> {
    let ssz_decode = |b: &[u8]| {
        ExecutionPayloadV3::from_ssz_bytes(b)
            .map_err(|e| eyre::eyre!("Failed to decode execution payload: {e:?}"))
    };
    if !lean_lane {
        return Ok((ssz_decode(bytes)?, None));
    }
    let prefix = le_u64_at(bytes, 0)
        .map_err(|_| eyre::eyre!("lane frame too short to contain a length prefix"))?;
    let flags = prefix & !LANE_LEN_MASK;
    if flags & !LEAN_LANE_BIT != 0 {
        return Err(eyre::eyre!("unknown lane frame flags {flags:#x}"));
    }
    let evm_end = usize::try_from(prefix & LANE_LEN_MASK)
        .ok()
        .and_then(|len| len.checked_add(8))
        .filter(|&end| end <= bytes.len())
        .ok_or_else(|| eyre::eyre!("invalid EVM payload length prefix"))?;
    let execution_payload = ssz_decode(&bytes[8..evm_end])?;
    let trailer = &bytes[evm_end..];
    if flags & LEAN_LANE_BIT != 0 {
        return Ok((
            execution_payload,
            Some(LeanLanePayload::new(trailer.to_vec())?),
        ));
    }
    if !trailer.is_empty() {
        return Err(eyre::eyre!(
            "EVM-only lane frame has {} trailing bytes",
            trailer.len()
        ));
    }
    Ok((execution_payload, None))
}

/// Lean block bytes in the canonical layout, for tests.
#[cfg(any(test, feature = "test-utils"))]
pub fn test_lean_block_bytes(
    parent: BlockHash,
    number: u64,
    timestamp_ms: u64,
    txs: &[&[u8]],
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(parent.as_slice());
    b.extend_from_slice(&number.to_le_bytes());
    b.extend_from_slice(&timestamp_ms.to_le_bytes());
    b.extend_from_slice(&u32::try_from(txs.len()).expect("tx count").to_le_bytes());
    for tx in txs {
        b.extend_from_slice(&u32::try_from(tx.len()).expect("tx len").to_le_bytes());
        b.extend_from_slice(tx);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, B256};
    use alloy_primitives::hex;

    fn payload(seed: u8) -> ExecutionPayloadV3 {
        ExecutionPayloadV3::from_block_unchecked(B256::repeat_byte(seed), &Block::default())
    }

    fn lean_bytes(parent: u8, number: u64, ts_ms: u64, txs: &[&[u8]]) -> Vec<u8> {
        test_lean_block_bytes(B256::repeat_byte(parent), number, ts_ms, txs)
    }

    fn lean(txs: &[&[u8]]) -> LeanLanePayload {
        LeanLanePayload::new(lean_bytes(0xAA, 3, 500, txs)).unwrap()
    }

    #[test]
    fn lean_commitment_is_content_sensitive_and_binds_the_tx_framing() {
        let a = decode_lean_block(&lean_bytes(0xAA, 7, 1000, &[b"tx-one", b"tx-two"])).unwrap();
        assert_eq!((a.number, a.timestamp_ms, a.tx_count), (7, 1000, 2));
        for variant in [
            lean_bytes(0xAB, 7, 1000, &[b"tx-one", b"tx-two"]),
            lean_bytes(0xAA, 8, 1000, &[b"tx-one", b"tx-two"]),
            lean_bytes(0xAA, 7, 1001, &[b"tx-one", b"tx-two"]),
            lean_bytes(0xAA, 7, 1000, &[b"tx-one", b"tx-tWo"]),
            lean_bytes(0xAA, 7, 1000, &[b"tx-one"]),
            // Same concatenated bodies, different boundaries: must not collide.
            lean_bytes(0xAA, 7, 1000, &[b"tx-onetx-two"]),
        ] {
            assert_ne!(
                decode_lean_block(&variant).unwrap().commitment,
                a.commitment
            );
        }
    }

    #[test]
    fn lean_decode_rejects_malformed() {
        let good = lean_bytes(0xAA, 1, 1, &[b"abc"]);
        assert!(decode_lean_block(&good).is_ok());

        let mut trailing = good.clone();
        trailing.push(0);
        let mut too_many = lean_bytes(0xAA, 1, 1, &[]);
        too_many[48..52].copy_from_slice(&200_001u32.to_le_bytes());
        let mut short_count = lean_bytes(0xAA, 1, 1, &[b"abc"]);
        short_count[48..52].copy_from_slice(&2u32.to_le_bytes());

        for bad in [
            trailing.as_slice(),
            good.split_last().unwrap().1,
            &good[..40],
            too_many.as_slice(),
            short_count.as_slice(),
        ] {
            assert!(decode_lean_block(bad).is_err());
        }
    }

    /// Exact bytes and commitments produced by the lean node's test-vector
    /// generator (chain 1338). If either side changes the wire format or the
    /// commitment formula, this breaks first.
    #[test]
    fn lean_commitment_cross_pins_against_lean_node() {
        let block1 = hex::decode(
            "ef24da0138bf37159737c3154c5e0a261b50dc3eea4b490975abc6b1215e57700100000000000000d204000000000000020000006400000050000000000100000000000000000000000000000000000000001105000000000000003758f90cf554424a708fa8a07fee665fb84ec41f52a550bff338e5e6aa301e51297d7d54501a9b330fde67ef30e27895cdda71c997c53240fe38d3e3b1195ccc009c00000050000000000300000000000000000000000000000000000000002207000000000000000000000000000000000000000000000000000033000000000000000000000000000000000000000000000000000000220900000000000000077619a741084e5037e9ac4bd54076d20f8afd4190507c875c161c6304429bb33192dc544fe674f0b1876eb7045284569e9db60627338fe30131bc8a427c876f01",
        )
        .unwrap();
        let b1 = decode_lean_block(&block1).unwrap();
        assert_eq!((b1.number, b1.timestamp_ms, b1.tx_count), (1, 1234, 2));
        assert_eq!(
            format!("{:?}", b1.commitment),
            "0x00e809a870be630b8dd454d74ef8d4bfa3f87f80daa8fac654520bb3874439a4"
        );
        let empty = hex::decode(
            "00e809a870be630b8dd454d74ef8d4bfa3f87f80daa8fac654520bb3874439a40200000000000000dc0500000000000000000000",
        )
        .unwrap();
        let b2 = decode_lean_block(&empty).unwrap();
        assert_eq!(b2.parent, b1.commitment);
        assert_eq!((b2.number, b2.tx_count), (2, 0));
        assert_eq!(
            format!("{:?}", b2.commitment),
            "0x05bbfd6d78e6242a68ee6e45076cf707b3705cfce97ec42b52481d1063827944"
        );
    }

    /// Flag off: the value is the stock SSZ encoding, byte for byte.
    #[test]
    fn encode_value_flag_off_is_stock_ssz() {
        let evm = payload(0x11);
        let bytes = encode_value(&evm, None, false);
        assert_eq!(bytes, evm.as_ssz_bytes());
        assert_eq!(decode_value(&bytes, false).unwrap(), (evm, None));
    }

    #[test]
    fn encode_value_flag_on_round_trips_with_and_without_a_lean_block() {
        let evm = payload(0x11);
        let lane = lean(&[b"tx-a", b"tx-b"]);
        for lean in [None, Some(lane)] {
            let bytes = encode_value(&evm, lean.as_ref(), true);
            let prefix = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            assert_eq!(prefix & LANE_LEN_MASK, evm.as_ssz_bytes().len() as u64);
            assert_eq!(prefix & LEAN_LANE_BIT != 0, lean.is_some());
            assert_eq!(decode_value(&bytes, true).unwrap(), (evm.clone(), lean));
        }
    }

    #[test]
    fn encode_value_never_drops_a_lean_payload() {
        let evm = payload(0x11);
        let lane = lean(&[]);
        assert_eq!(
            encode_value(&evm, Some(&lane), false),
            encode_value(&evm, Some(&lane), true)
        );
    }

    /// The format is the node's own flag: each side rejects the other's bytes,
    /// and unknown flag bits or trailing bytes fail closed.
    #[test]
    fn decode_value_fails_closed() {
        let evm = payload(0x11);
        let lane = lean(&[b"tx"]);
        assert!(decode_value(&evm.as_ssz_bytes(), true).is_err());
        assert!(decode_value(&encode_value(&evm, Some(&lane), true), false).is_err());

        let mut unknown_flag = encode_value(&evm, None, true);
        unknown_flag[7] |= 0x80;
        assert!(decode_value(&unknown_flag, true).is_err());

        let mut trailing = encode_value(&evm, None, true);
        trailing.push(0);
        assert!(decode_value(&trailing, true).is_err());

        let mut bad_lean = encode_value(&evm, Some(&lane), true);
        bad_lean.push(0);
        assert!(decode_value(&bad_lean, true).is_err());
    }
}
