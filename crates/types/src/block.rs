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
    /// Payment lane as LEAN block bytes (`ARC_PAYMENT_LEAN_LANE`). Mutually
    /// exclusive with `payment_payload`. NOT part of the SSZ store form — the
    /// decided store stays EVM-only (the lean node is canonical for lane data,
    /// the certificate's value_id is the authoritative commitment; same pattern
    /// as the reth payment lane). Travels in proposals via `frame_lanes_lean`.
    pub lean_payload: Option<LeanLanePayload>,
}

/// Lean lane bytes plus their decoded+verified view. Construction ALWAYS
/// decodes (strict) and recomputes the commitment — carrying this type means
/// the bytes have been validated.
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
        debug_assert!(
            !(self.payment_payload.is_some() && self.lean_payload.is_some()),
            "payment_payload and lean_payload are mutually exclusive"
        );
        commit_lanes(self.block_hash(), self.payment_lane_commitment())
    }

    /// The payment-lane execution block hash, if a payment lane is present.
    pub fn payment_block_hash(&self) -> Option<BlockHash> {
        self.payment_payload
            .as_ref()
            .map(|p| p.payload_inner.payload_inner.block_hash)
    }

    /// The payment lane's commitment for value_id purposes: the reth lane's
    /// block hash, or the lean lane's recomputed commitment.
    pub fn payment_lane_commitment(&self) -> Option<BlockHash> {
        self.payment_block_hash()
            .or_else(|| self.lean_payload.as_ref().map(|l| l.commitment()))
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

    /// Lean-lane variant of [`DecidedBlock::new`]: the payment lane is a lean
    /// commitment (recomputed from validated bytes by the caller), not an
    /// ExecutionPayloadV3. Panics if `commit_lanes(evm, lane)` does not match
    /// the certificate — same invariant as `new`.
    pub fn new_with_lane_commitment(
        execution_payload: ExecutionPayloadV3,
        lane_commitment: Option<BlockHash>,
        certificate: CommitCertificate<ArcContext>,
    ) -> Self {
        let evm_block_hash = execution_payload.payload_inner.payload_inner.block_hash;
        let value_id = commit_lanes(evm_block_hash, lane_commitment);
        let certificate_value_id = certificate.value_id.block_hash();
        assert_eq!(
            value_id, certificate_value_id,
            "decided lanes do not reproduce the certified value_id \
             (evm {evm_block_hash}, lane {lane_commitment:?})"
        );
        Self {
            execution_payload,
            payment_payload: None,
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

/// Bit 62 of the EVM length prefix marks a LEAN payment section: the payment
/// lane bytes are canonical lean-lane block bytes (see [`decode_lean_block`]),
/// NOT an SSZ `ExecutionPayloadV3`. Like the compact bit, a legacy decoder
/// sees an absurd length and fails its bounds check — fail-closed. Both bits
/// set is invalid.
pub const LEAN_LANE_BIT: u64 = 1 << 62;

/// DoS bound on lean block tx count, checked before any per-tx work.
pub const MAX_LEAN_TXS: usize = 200_000;

/// Decoded, VERIFIED view of canonical lean-lane block bytes:
/// `[parent 32B][number u64 LE][timestamp_ms u64 LE][n_txs u32 LE]([len u32 LE][tx])*`
/// with `commitment = keccak256(parent ‖ number LE ‖ timestamp_ms LE ‖ keccak256(cat txs))`.
/// The commitment is always RECOMPUTED from content here — never trusted from
/// the wire (the analog of rebuilding the EVM header in structural validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeanBlockRef {
    pub parent: BlockHash,
    pub number: u64,
    pub timestamp_ms: u64,
    pub tx_count: u32,
    pub commitment: BlockHash,
}

/// Strict decoder for lean block bytes (trailing bytes rejected, tx count
/// capped). Returns the recomputed commitment alongside the header fields.
pub fn decode_lean_block(bytes: &[u8]) -> eyre::Result<LeanBlockRef> {
    const HDR: usize = 32 + 8 + 8 + 4;
    if bytes.len() < HDR {
        return Err(eyre::eyre!("lean block bytes too short ({})", bytes.len()));
    }
    let parent = BlockHash::from_slice(&bytes[..32]);
    let number = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
    let timestamp_ms = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
    let tx_count = u32::from_le_bytes(bytes[48..52].try_into().unwrap());
    if tx_count as usize > MAX_LEAN_TXS {
        return Err(eyre::eyre!("lean block carries {tx_count} txs (cap {MAX_LEAN_TXS})"));
    }
    let mut off = HDR;
    for i in 0..tx_count {
        let len_end = off
            .checked_add(4)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| eyre::eyre!("lean block truncated at tx {i} length"))?;
        let len = u32::from_le_bytes(bytes[off..len_end].try_into().unwrap()) as usize;
        let tx_end = len_end
            .checked_add(len)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| eyre::eyre!("lean block truncated at tx {i} body (len {len})"))?;
        off = tx_end;
    }
    if off != bytes.len() {
        return Err(eyre::eyre!(
            "lean block has {} trailing bytes after {tx_count} txs",
            bytes.len() - off
        ));
    }
    // txs_hash binds the ENTIRE framed tx section (count + per-tx lengths +
    // bodies), NOT the concatenated bodies. Concatenation-only binding is a
    // consensus hole: two different boundary-framings of the same body bytes
    // would share a commitment yet decode to different tx lists, and under
    // total-STF both "execute" — same commitment, divergent state (a malicious
    // sync peer could exploit this). Binding the framing closes it.
    let txs_hash = {
        let mut h = Keccak256::new();
        h.update(&bytes[48..]);
        h.finalize()
    };
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

/// Lean variant of [`frame_lanes`]:
/// `[u64-LE len(evm) | LEAN_LANE_BIT] [evm SSZ] [lean block bytes]`.
pub fn frame_lanes_lean(execution_payload: &ExecutionPayloadV3, lean_bytes: &[u8]) -> Vec<u8> {
    let evm = execution_payload.as_ssz_bytes();
    let mut buf = Vec::with_capacity(8 + evm.len() + lean_bytes.len());
    buf.extend_from_slice(&((evm.len() as u64) | LEAN_LANE_BIT).to_le_bytes());
    buf.extend_from_slice(&evm);
    buf.extend_from_slice(lean_bytes);
    buf
}

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
    /// EVM full + the payment lane as canonical LEAN block bytes
    /// (`ARC_PAYMENT_LEAN_LANE`). `lean` is the decoded+verified view; the raw
    /// bytes are kept verbatim for the newBlock feed and re-framing.
    LeanPayment {
        execution_payload: ExecutionPayloadV3,
        lean: LeanBlockRef,
        lean_bytes: Vec<u8>,
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
    if raw_len & LEAN_LANE_BIT != 0 {
        if raw_len & COMPACT_LANE_BIT != 0 {
            return Err(eyre::eyre!("lane frame has both LEAN and COMPACT bits set"));
        }
        let len_evm = (raw_len & !LEAN_LANE_BIT) as usize;
        let evm_end = 8usize
            .checked_add(len_evm)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| eyre::eyre!("invalid lean evm length prefix"))?;
        let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&bytes[8..evm_end])
            .map_err(|e| eyre::eyre!("Failed to decode execution payload: {e:?}"))?;
        let lean_bytes = bytes[evm_end..].to_vec();
        let lean = decode_lean_block(&lean_bytes)?;
        return Ok(LaneFrame::LeanPayment {
            execution_payload,
            lean,
            lean_bytes,
        });
    }
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
            lean_payload: None,
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

    /// Builds canonical lean block bytes for tests (mirrors the lean-lane-node
    /// encoding: [parent][number LE][ts_ms LE][n u32 LE]([len u32 LE][tx])*).
    fn lean_bytes(parent: u8, number: u64, ts_ms: u64, txs: &[&[u8]]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(B256::repeat_byte(parent).as_slice());
        b.extend_from_slice(&number.to_le_bytes());
        b.extend_from_slice(&ts_ms.to_le_bytes());
        b.extend_from_slice(&(txs.len() as u32).to_le_bytes());
        for t in txs {
            b.extend_from_slice(&(t.len() as u32).to_le_bytes());
            b.extend_from_slice(t);
        }
        b
    }

    #[test]
    fn lean_decode_recomputes_commitment_and_is_content_sensitive() {
        let a = decode_lean_block(&lean_bytes(0xAA, 7, 1000, &[b"tx-one", b"tx-two"])).unwrap();
        assert_eq!((a.number, a.timestamp_ms, a.tx_count), (7, 1000, 2));
        // Any content change moves the commitment.
        for variant in [
            lean_bytes(0xAB, 7, 1000, &[b"tx-one", b"tx-two"]),
            lean_bytes(0xAA, 8, 1000, &[b"tx-one", b"tx-two"]),
            lean_bytes(0xAA, 7, 1001, &[b"tx-one", b"tx-two"]),
            lean_bytes(0xAA, 7, 1000, &[b"tx-one", b"tx-tWo"]),
            lean_bytes(0xAA, 7, 1000, &[b"tx-one"]),
        ] {
            assert_ne!(decode_lean_block(&variant).unwrap().commitment, a.commitment);
        }
        // SECURITY: boundary shifts with identical concatenated bodies MUST
        // move the commitment — otherwise two framings of the same bytes share
        // a commitment but decode to different tx lists, and total-STF executes
        // both (same commitment, divergent state; exploitable via sync).
        let b = decode_lean_block(&lean_bytes(0xAA, 7, 1000, &[b"tx-onetx-two"])).unwrap();
        assert_ne!(b.commitment, a.commitment, "commitment must bind tx FRAMING, not just bodies");
    }

    #[test]
    fn lean_decode_rejects_malformed() {
        let good = lean_bytes(0xAA, 1, 1, &[b"abc"]);
        assert!(decode_lean_block(&good).is_ok());
        // trailing garbage
        let mut t = good.clone();
        t.push(0);
        assert!(decode_lean_block(&t).is_err());
        // truncated tx body
        assert!(decode_lean_block(&good[..good.len() - 1]).is_err());
        // truncated header
        assert!(decode_lean_block(&good[..40]).is_err());
        // absurd tx count with no bodies
        let mut c = lean_bytes(0xAA, 1, 1, &[]);
        let n = (MAX_LEAN_TXS as u32 + 1).to_le_bytes();
        c[48..52].copy_from_slice(&n);
        assert!(decode_lean_block(&c).is_err());
        // claimed count larger than actual bodies
        let mut d = lean_bytes(0xAA, 1, 1, &[b"abc"]);
        d[48..52].copy_from_slice(&2u32.to_le_bytes());
        assert!(decode_lean_block(&d).is_err());
    }

    #[test]
    fn lean_frame_round_trips_and_legacy_paths_fail_closed() {
        let evm = payload(0x11);
        let lb = lean_bytes(0xAA, 3, 500, &[b"tx-a", b"tx-b", b"tx-c"]);
        let framed = frame_lanes_lean(&evm, &lb);
        match unframe_lanes_any(&framed).unwrap() {
            LaneFrame::LeanPayment { execution_payload, lean, lean_bytes } => {
                assert_eq!(execution_payload, evm);
                assert_eq!(lean_bytes, lb);
                assert_eq!(lean, decode_lean_block(&lb).unwrap());
            }
            other => panic!("expected LeanPayment, got {other:?}"),
        }
        // The full-format-only decoder must refuse a lean frame (fail-closed),
        // exactly like it refuses compact frames.
        assert!(unframe_lanes(&framed).is_err());
        // Both format bits set is invalid.
        let mut both = framed.clone();
        let raw = u64::from_le_bytes(both[..8].try_into().unwrap()) | COMPACT_LANE_BIT;
        both[..8].copy_from_slice(&raw.to_le_bytes());
        assert!(unframe_lanes_any(&both).is_err());
    }

    /// CROSS-IMPLEMENTATION PIN: exact blockBytes + v2 commitments produced by
    /// the lean lane node's `gen_vector` example (~/reth-fork, chain 1338).
    /// If either side changes the wire format or commitment formula, this
    /// breaks FIRST. Genesis = keccak("ARC_LEAN_LANE_GENESIS" ‖ 1338 BE).
    #[test]
    fn lean_commitment_cross_pins_against_lean_node() {
        use alloy_primitives::hex;
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
        assert_eq!(b2.parent, b1.commitment, "empty block links to block 1");
        assert_eq!((b2.number, b2.tx_count), (2, 0));
        assert_eq!(
            format!("{:?}", b2.commitment),
            "0x05bbfd6d78e6242a68ee6e45076cf707b3705cfce97ec42b52481d1063827944"
        );
    }

    /// Lean-lane equivocation guard analog: same EVM lane, different lean bytes
    /// ⇒ different commitment ⇒ different value_id through commit_lanes.
    #[test]
    fn lean_commitment_binds_value_id() {
        let evm_hash = BlockHash::repeat_byte(0x11);
        let c1 = decode_lean_block(&lean_bytes(0xAA, 3, 500, &[b"tx-a"])).unwrap().commitment;
        let c2 = decode_lean_block(&lean_bytes(0xAA, 3, 500, &[b"tx-b"])).unwrap().commitment;
        assert_ne!(commit_lanes(evm_hash, Some(c1)), commit_lanes(evm_hash, Some(c2)));
        assert_ne!(commit_lanes(evm_hash, Some(c1)), evm_hash);
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
