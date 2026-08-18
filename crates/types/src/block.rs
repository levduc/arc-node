// Copyright 2025 Circle Internet Group, Inc. All rights reserved.
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

use alloy_rpc_types_engine::ExecutionPayloadV3;
use bytesize::ByteSize;
use sha3::{Digest, Keccak256};
use ssz::{Decode, Encode};

use malachitebft_app_channel::app::types::core::{CommitCertificate, Round, Validity};
use malachitebft_app_channel::app::types::{LocallyProposedValue, ProposedValue};

use crate::ssz::{SszBlock, SszSignature};
use crate::{signing::Signature, Address, ArcContext, BlockHash, Height, Value};

/// A block as seen by the consensus layer.
///
/// This includes the execution payload, the metadata required for consensus,
/// and the signature for its proposal parts.
/// Note that this is a block that has been proposed but not yet decided, that is,
/// consensus has not yet been reached on it. Therefore, it might not become the
/// next head of the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsensusBlock {
    pub height: Height,
    pub round: Round,
    pub valid_round: Round,
    pub proposer: Address,
    pub validity: Validity,
    pub execution_payload: ExecutionPayloadV3,
    pub signature: Option<Signature>,
    /// Payment-lane execution payload (second EL). `None` for single-EL blocks.
    pub payment_payload: Option<ExecutionPayloadV3>,
}

impl ConsensusBlock {
    /// Returns the EVM execution block hash.
    ///
    /// This is used for all execution-layer interactions (parent lookup,
    /// forkchoice, RPC) and is NOT the value consensus votes on when a
    /// payment lane is present — see [`ConsensusBlock::value_id`].
    pub fn block_hash(&self) -> BlockHash {
        self.execution_payload
            .payload_inner
            .payload_inner
            .block_hash
    }

    /// Returns the consensus commitment: the value BFT votes on and certifies.
    ///
    /// It binds BOTH lanes so an equivocating proposer cannot fork the payment
    /// lane under a valid EVM commit certificate. For single-EL blocks (no
    /// payment lane) this equals [`ConsensusBlock::block_hash`] byte-for-byte,
    /// so existing chains and the single-EL path are unaffected.
    pub fn value_id(&self) -> BlockHash {
        commit_lanes(self.block_hash(), self.payment_block_hash())
    }

    /// The payment-lane execution block hash, if a payment lane is present.
    pub fn payment_block_hash(&self) -> Option<BlockHash> {
        self.payment_payload
            .as_ref()
            .map(|p| p.payload_inner.payload_inner.block_hash)
    }

    /// Returns the size of the block in bytes when encoded using SSZ.
    pub fn size_bytes(&self) -> ByteSize {
        // TODO: Cache this
        ByteSize::b(block_as_ssz_data(self).ssz_bytes_len() as u64)
    }

    /// Returns the size of the execution payload in bytes when encoded using SSZ.
    pub fn payload_size(&self) -> ByteSize {
        ByteSize::b(self.execution_payload.ssz_bytes_len() as u64)
    }
}

impl From<&ConsensusBlock> for ProposedValue<ArcContext> {
    fn from(block: &ConsensusBlock) -> Self {
        ProposedValue {
            height: block.height,
            round: block.round,
            proposer: block.proposer,
            valid_round: block.valid_round,
            value: Value::new(block.value_id()),
            validity: block.validity,
        }
    }
}

impl From<&ConsensusBlock> for LocallyProposedValue<ArcContext> {
    fn from(block: &ConsensusBlock) -> Self {
        LocallyProposedValue {
            height: block.height,
            round: block.round,
            value: Value::new(block.value_id()),
        }
    }
}

/// Converts a ConsensusBlock into a tuple suitable for SSZ encoding
pub fn block_as_ssz_data(block: &ConsensusBlock) -> SszBlock<&'_ ExecutionPayloadV3> {
    (
        block.height.as_u64(),
        block.round.as_u32(),
        block.valid_round.as_u32(),
        block.proposer.to_alloy_address(),
        block.validity.is_valid(),
        &block.execution_payload,
        block.signature.map(SszSignature),
        block.payment_payload.as_ref(),
    )
}

/// Decided block, ie. an execution payload together with its commit certificate.
/// A decided block is a block for which consensus has been reached and therefore
/// it can't be removed from the chain.
#[derive(Clone, Debug)]
pub struct DecidedBlock {
    pub execution_payload: ExecutionPayloadV3,
    /// Payment-lane execution payload (second EL). `None` for single-EL blocks.
    pub payment_payload: Option<ExecutionPayloadV3>,
    pub certificate: CommitCertificate<ArcContext>,
}

impl DecidedBlock {
    /// Creates a new decided block from the lane payloads and a commit certificate.
    /// The commitment over both lanes must match the value id in the commit
    /// certificate (see [`ConsensusBlock::value_id`]).
    pub fn new(
        execution_payload: ExecutionPayloadV3,
        payment_payload: Option<ExecutionPayloadV3>,
        certificate: CommitCertificate<ArcContext>,
    ) -> Self {
        let evm_block_hash = execution_payload.payload_inner.payload_inner.block_hash;
        let payment_block_hash = payment_payload
            .as_ref()
            .map(|p| p.payload_inner.payload_inner.block_hash);
        let value_id = commit_lanes(evm_block_hash, payment_block_hash);
        let certificate_value_id = certificate.value_id.block_hash();

        assert_eq!(
            value_id, certificate_value_id,
            "Commitment over the execution payloads does not match the value id in the commit certificate"
        );

        Self {
            execution_payload,
            payment_payload,
            certificate,
        }
    }

    /// Reconstructs a decided block from persisted CL state.
    ///
    /// The CL decided store caches only the EVM payload; the authoritative
    /// commitment over both lanes is `certificate.value_id`, and the payment
    /// block is persisted canonically by the payment EL. Unlike
    /// [`DecidedBlock::new`], this does NOT recompute/verify the commitment,
    /// because the payment payload is not available from the CL store.
    pub fn from_stored_evm_only(
        execution_payload: ExecutionPayloadV3,
        certificate: CommitCertificate<ArcContext>,
    ) -> Self {
        Self {
            execution_payload,
            payment_payload: None,
            certificate,
        }
    }

    /// Returns the height at which the block was decided.
    pub fn height(&self) -> Height {
        self.certificate.height
    }
}

/// Computes the consensus commitment over both lanes.
///
/// When there is no payment lane the commitment is exactly the EVM block hash,
/// preserving backward compatibility with single-EL blocks. When a payment lane
/// is present the commitment is `keccak256(evm_block_hash ‖ payment_block_hash)`.
///
/// This is the single source of truth for the commitment: both
/// [`ConsensusBlock::value_id`] and [`DecidedBlock::new`] go through it so they
/// can never drift.
pub fn commit_lanes(evm_block_hash: BlockHash, payment_block_hash: Option<BlockHash>) -> BlockHash {
    match payment_block_hash {
        None => evm_block_hash,
        Some(payment_block_hash) => {
            let mut hasher = Keccak256::new();
            hasher.update(evm_block_hash.as_slice());
            hasher.update(payment_block_hash.as_slice());
            BlockHash::from_slice(&hasher.finalize())
        }
    }
}

/// Length-frames the two lane payloads into a single byte buffer:
/// `[u64-LE len(evm)] [evm SSZ] [payment SSZ (optional)]`.
///
/// The length prefix lets the decoder split the two lanes; an absent payment
/// lane produces no trailer. Shared by the proposal-streaming path and the
/// value-sync path so both produce and consume identical bytes.
pub fn frame_lanes(
    execution_payload: &ExecutionPayloadV3,
    payment_payload: Option<&ExecutionPayloadV3>,
) -> Vec<u8> {
    let evm = execution_payload.as_ssz_bytes();
    let mut buf = Vec::with_capacity(8 + evm.len());
    buf.extend_from_slice(&(evm.len() as u64).to_le_bytes());
    buf.extend_from_slice(&evm);
    if let Some(payment) = payment_payload {
        buf.extend_from_slice(&payment.as_ssz_bytes());
    }
    buf
}

/// Inverse of [`frame_lanes`]: splits a framed buffer back into the EVM payload
/// and the optional payment payload.
pub fn unframe_lanes(
    bytes: &[u8],
) -> eyre::Result<(ExecutionPayloadV3, Option<ExecutionPayloadV3>)> {
    if bytes.len() < 8 {
        return Err(eyre::eyre!("lane bytes too short to contain length prefix"));
    }
    let raw_len = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    if raw_len & COMPACT_LANE_BIT != 0 {
        return Err(eyre::eyre!(
            "compact lane frame (ARC_COMPACT_PAYMENT_PROPOSALS) on a full-format-only \
             path — this node/path cannot decode compact proposals"
        ));
    }
    let len_evm = raw_len as usize;
    let evm_end = 8usize
        .checked_add(len_evm)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| eyre::eyre!("invalid evm payload length prefix"))?;
    let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&bytes[8..evm_end])
        .map_err(|e| eyre::eyre!("Failed to decode execution payload: {e:?}"))?;
    let payment_payload = if bytes.len() > evm_end {
        Some(
            ExecutionPayloadV3::from_ssz_bytes(&bytes[evm_end..])
                .map_err(|e| eyre::eyre!("Failed to decode payment payload: {e:?}"))?,
        )
    } else {
        None
    };
    Ok((execution_payload, payment_payload))
}

/// Bit 63 of the EVM length prefix marks a COMPACT payment section
/// (`frame_lanes_compact`). Real payload lengths can never approach 2^63, so a
/// legacy decoder sees an absurd length and fails its bounds check — fail-closed.
pub const COMPACT_LANE_BIT: u64 = 1 << 63;

/// Hard cap on the number of tx hashes a compact frame may carry, checked
/// BEFORE any EL fetch (DoS bound). 1 Ggas / 21k gas = 47,619 transfers; 200k
/// leaves generous headroom.
pub const MAX_COMPACT_TX_HASHES: usize = 200_000;

/// Decoded form of a framed lane buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneFrame {
    /// Full payloads for both lanes (payment optional) — the legacy format.
    Full(ExecutionPayloadV3, Option<ExecutionPayloadV3>),
    /// EVM full + payment as header-with-empty-txs plus the tx hash list.
    /// The receiver reconstructs the full payment payload from its EL pool;
    /// the structural/newPayload block-hash check verifies the reconstruction.
    CompactPayment {
        execution_payload: ExecutionPayloadV3,
        payment_header: ExecutionPayloadV3,
        tx_hashes: Vec<BlockHash>,
    },
}

/// Compact variant of [`frame_lanes`]:
/// `[u64-LE len(evm) | COMPACT_LANE_BIT] [evm SSZ]
///  [u64-LE len(payment_header)] [payment header SSZ, transactions stripped]
///  [k * 32B tx hashes]`
///
/// Tx hashes are `keccak256` of the raw payload tx bytes — exactly the tx hash
/// for both legacy (RLP) and EIP-2718 typed (envelope incl. type byte) txs.
/// LIVE-PROPOSAL WIRE FORMAT ONLY: stores and value-sync always carry full
/// payloads (decided txs leave the pools).
pub fn frame_lanes_compact(
    execution_payload: &ExecutionPayloadV3,
    payment_payload: &ExecutionPayloadV3,
) -> Vec<u8> {
    let evm = execution_payload.as_ssz_bytes();
    let mut header_only = payment_payload.clone();
    header_only.payload_inner.payload_inner.transactions = Vec::new();
    let header_ssz = header_only.as_ssz_bytes();
    let txs = &payment_payload.payload_inner.payload_inner.transactions;

    let mut buf = Vec::with_capacity(16 + evm.len() + header_ssz.len() + txs.len() * 32);
    buf.extend_from_slice(&((evm.len() as u64) | COMPACT_LANE_BIT).to_le_bytes());
    buf.extend_from_slice(&evm);
    buf.extend_from_slice(&(header_ssz.len() as u64).to_le_bytes());
    buf.extend_from_slice(&header_ssz);
    for tx in txs {
        let mut hasher = Keccak256::new();
        hasher.update(tx.as_ref());
        buf.extend_from_slice(&hasher.finalize());
    }
    buf
}

/// Decodes a framed lane buffer in EITHER format (full or compact).
/// Decoding compact frames is unconditional (not flag-gated) so a mixed
/// new-binary fleet interoperates regardless of per-node emission flags.
pub fn unframe_lanes_any(bytes: &[u8]) -> eyre::Result<LaneFrame> {
    if bytes.len() < 8 {
        return Err(eyre::eyre!("lane bytes too short to contain length prefix"));
    }
    let raw_len = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    if raw_len & COMPACT_LANE_BIT == 0 {
        let (evm, pay) = unframe_lanes(bytes)?;
        return Ok(LaneFrame::Full(evm, pay));
    }

    let len_evm = (raw_len & !COMPACT_LANE_BIT) as usize;
    let evm_end = 8usize
        .checked_add(len_evm)
        .filter(|&e| e + 8 <= bytes.len())
        .ok_or_else(|| eyre::eyre!("invalid compact evm length prefix"))?;
    let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&bytes[8..evm_end])
        .map_err(|e| eyre::eyre!("Failed to decode execution payload: {e:?}"))?;

    let len_hdr =
        u64::from_le_bytes(bytes[evm_end..evm_end + 8].try_into().unwrap()) as usize;
    let hdr_end = (evm_end + 8)
        .checked_add(len_hdr)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| eyre::eyre!("invalid compact payment header length prefix"))?;
    let payment_header = ExecutionPayloadV3::from_ssz_bytes(&bytes[evm_end + 8..hdr_end])
        .map_err(|e| eyre::eyre!("Failed to decode compact payment header: {e:?}"))?;
    if !payment_header
        .payload_inner
        .payload_inner
        .transactions
        .is_empty()
    {
        return Err(eyre::eyre!(
            "compact payment header must carry no transactions"
        ));
    }

    let tail = &bytes[hdr_end..];
    if tail.len() % 32 != 0 {
        return Err(eyre::eyre!(
            "compact tx hash section length {} not a multiple of 32",
            tail.len()
        ));
    }
    let k = tail.len() / 32;
    if k > MAX_COMPACT_TX_HASHES {
        return Err(eyre::eyre!(
            "compact frame carries {k} tx hashes (cap {MAX_COMPACT_TX_HASHES})"
        ));
    }
    let tx_hashes = tail
        .chunks_exact(32)
        .map(BlockHash::from_slice)
        .collect();

    Ok(LaneFrame::CompactPayment {
        execution_payload,
        payment_header,
        tx_hashes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Address, Height, Round};
    use alloy_primitives::{Bloom, Bytes as AlloyBytes, B256, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2};
    use malachitebft_app_channel::app::types::core::Validity;

    /// Builds a payload whose block hash (and state root) is keyed by `seed`, so
    /// distinct seeds are distinguishable lanes.
    fn payload(seed: u8) -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: alloy_primitives::Address::ZERO,
                    state_root: B256::repeat_byte(seed),
                    receipts_root: B256::ZERO,
                    logs_bloom: Bloom::default(),
                    prev_randao: B256::ZERO,
                    block_number: 0,
                    gas_limit: 0,
                    gas_used: 0,
                    timestamp: 0,
                    extra_data: AlloyBytes::default(),
                    base_fee_per_gas: U256::from(1u64),
                    block_hash: B256::repeat_byte(seed),
                    transactions: vec![],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    fn block(evm: ExecutionPayloadV3, payment: Option<ExecutionPayloadV3>) -> ConsensusBlock {
        ConsensusBlock {
            height: Height::new(1),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: Address::new([0u8; 20]),
            validity: Validity::Valid,
            execution_payload: evm,
            signature: None,
            payment_payload: payment,
        }
    }

    /// Single-EL blocks must commit to exactly the EVM block hash, so existing
    /// chains and the single-EL path are byte-for-byte unaffected.
    #[test]
    fn value_id_equals_block_hash_without_payment_lane() {
        let b = block(payload(0x11), None);
        assert_eq!(b.value_id(), b.block_hash());
    }

    /// With a payment lane, the commitment binds BOTH lanes and therefore differs
    /// from the bare EVM block hash.
    #[test]
    fn value_id_binds_payment_lane() {
        let evm = payload(0x11);
        let pay = payload(0x22);
        let b = block(evm, Some(pay));
        assert_ne!(
            b.value_id(),
            b.block_hash(),
            "value_id must not collapse to the EVM hash when a payment lane is present"
        );
    }

    /// The equivocation guard: two blocks with the SAME EVM lane but DIFFERENT
    /// payment lanes must produce DIFFERENT value_ids. Otherwise a proposer could
    /// fork the payment lane under one EVM commit certificate.
    #[test]
    fn value_id_changes_when_only_payment_lane_changes() {
        let evm = payload(0x11);
        let b1 = block(evm.clone(), Some(payload(0x22)));
        let b2 = block(evm.clone(), Some(payload(0x33)));
        assert_eq!(b1.block_hash(), b2.block_hash(), "test setup: EVM lane identical");
        assert_ne!(
            b1.payment_block_hash(),
            b2.payment_block_hash(),
            "test setup: payment lanes must differ"
        );
        assert_ne!(
            b1.value_id(),
            b2.value_id(),
            "same EVM lane + different payment lane must yield different commitments"
        );
    }

    /// `commit_lanes` is order-sensitive: swapping the two lane hashes yields a
    /// different commitment (guards against a symmetric-hash mistake).
    #[test]
    fn commit_lanes_is_order_sensitive() {
        let a = BlockHash::from_slice(&[0xAA; 32]);
        let b = BlockHash::from_slice(&[0xBB; 32]);
        assert_ne!(commit_lanes(a, Some(b)), commit_lanes(b, Some(a)));
    }

    /// A dual-lane block survives the value-sync framing round-trip with both
    /// lanes intact and the same value_id on both ends.
    #[test]
    fn frame_unframe_round_trips_both_lanes() {
        let evm = payload(0x11);
        let pay = payload(0x22);
        let framed = frame_lanes(&evm, Some(&pay));
        let (evm_out, pay_out) = unframe_lanes(&framed).unwrap();
        assert_eq!(evm_out, evm);
        assert_eq!(pay_out, Some(pay));
    }

    #[test]
    fn frame_unframe_round_trips_single_lane() {
        let evm = payload(0x11);
        let framed = frame_lanes(&evm, None);
        let (evm_out, pay_out) = unframe_lanes(&framed).unwrap();
        assert_eq!(evm_out, evm);
        assert_eq!(pay_out, None);
    }
}

#[cfg(test)]
mod compact_frame_tests {
    use super::*;
    use alloy_primitives::{Bloom, Bytes as AlloyBytes, B256, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2};

    fn payload_with_txs(seed: u8, txs: Vec<AlloyBytes>) -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::repeat_byte(seed),
                    fee_recipient: Default::default(),
                    state_root: B256::repeat_byte(seed.wrapping_add(1)),
                    receipts_root: B256::repeat_byte(seed.wrapping_add(2)),
                    logs_bloom: Bloom::default(),
                    prev_randao: B256::ZERO,
                    block_number: seed as u64,
                    gas_limit: 30_000_000,
                    gas_used: 21_000,
                    timestamp: 1_000 + seed as u64,
                    extra_data: AlloyBytes::default(),
                    base_fee_per_gas: U256::from(1u64),
                    block_hash: B256::repeat_byte(seed.wrapping_add(3)),
                    transactions: txs,
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    #[test]
    fn compact_round_trip() {
        let evm = payload_with_txs(0x10, vec![]);
        let txs = vec![
            AlloyBytes::from(vec![0x02, 0xde, 0xad]),
            AlloyBytes::from(vec![0x02, 0xbe, 0xef, 0x01]),
        ];
        let pay = payload_with_txs(0x20, txs.clone());
        let framed = frame_lanes_compact(&evm, &pay);
        match unframe_lanes_any(&framed).unwrap() {
            LaneFrame::CompactPayment {
                execution_payload,
                payment_header,
                tx_hashes,
            } => {
                assert_eq!(execution_payload, evm);
                assert!(payment_header
                    .payload_inner
                    .payload_inner
                    .transactions
                    .is_empty());
                assert_eq!(
                    payment_header.payload_inner.payload_inner.block_hash,
                    pay.payload_inner.payload_inner.block_hash
                );
                assert_eq!(tx_hashes.len(), 2);
                for (h, tx) in tx_hashes.iter().zip(&txs) {
                    let mut hasher = Keccak256::new();
                    hasher.update(tx.as_ref());
                    assert_eq!(h.as_slice(), &hasher.finalize()[..]);
                }
            }
            other => panic!("expected compact frame, got {other:?}"),
        }
    }

    #[test]
    fn full_frames_still_decode_via_any() {
        let evm = payload_with_txs(0x10, vec![AlloyBytes::from(vec![0x01])]);
        let pay = payload_with_txs(0x20, vec![AlloyBytes::from(vec![0x02])]);
        let framed = frame_lanes(&evm, Some(&pay));
        match unframe_lanes_any(&framed).unwrap() {
            LaneFrame::Full(e, Some(p)) => {
                assert_eq!(e, evm);
                assert_eq!(p, pay);
            }
            other => panic!("expected full frame, got {other:?}"),
        }
        // single-EL
        let framed = frame_lanes(&evm, None);
        assert!(matches!(
            unframe_lanes_any(&framed).unwrap(),
            LaneFrame::Full(_, None)
        ));
    }

    #[test]
    fn legacy_decoder_fails_closed_on_compact_and_names_the_flag() {
        let evm = payload_with_txs(0x10, vec![]);
        let pay = payload_with_txs(0x20, vec![AlloyBytes::from(vec![0x02, 0x01])]);
        let framed = frame_lanes_compact(&evm, &pay);
        let err = unframe_lanes(&framed).unwrap_err();
        assert!(err.to_string().contains("ARC_COMPACT_PAYMENT_PROPOSALS"));
    }

    #[test]
    fn compact_rejects_bad_hash_remainder() {
        let evm = payload_with_txs(0x10, vec![]);
        let pay = payload_with_txs(0x20, vec![AlloyBytes::from(vec![0x02, 0x01])]);
        let mut framed = frame_lanes_compact(&evm, &pay);
        framed.push(0xff); // 33-byte tail
        assert!(unframe_lanes_any(&framed).is_err());
    }

    #[test]
    fn flag_off_framing_byte_identical() {
        // frame_lanes untouched by the compact addition
        let evm = payload_with_txs(0x10, vec![AlloyBytes::from(vec![0x01])]);
        let pay = payload_with_txs(0x20, vec![AlloyBytes::from(vec![0x02])]);
        let framed = frame_lanes(&evm, Some(&pay));
        assert_eq!(
            u64::from_le_bytes(framed[..8].try_into().unwrap()) & COMPACT_LANE_BIT,
            0
        );
    }
}
