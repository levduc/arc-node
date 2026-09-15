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

use alloy_eips::eip7685::Requests;
use alloy_rpc_types_engine::{
    CancunPayloadFields, ExecutionData, ExecutionPayload, ExecutionPayloadSidecar,
    ExecutionPayloadV3, PayloadError, PraguePayloadFields,
};
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
    /// LEAN payment lane block (`ARC_PAYMENT_LEAN_LANE`): the lane node's
    /// canonical block bytes plus their decoded+verified view. `None` for
    /// single-lane blocks. NOT part of the SSZ store form — the lean node is
    /// canonical for lane data; the binding to consensus is the EVM header's
    /// `prev_randao` ([`Self::header_lean_commitment`],
    /// [`Self::lean_binding_ok`]), not the certified value id, which is
    /// always the plain EVM block hash. Travels in proposals and value-sync
    /// via [`frame_lanes`].
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
    /// Returns the block hash carried verbatim in the execution payload.
    ///
    /// Not recomputed from the payload, so it is untrusted; use
    /// [`Self::canonical_block_hash`] where a verified hash is required.
    pub fn self_reported_block_hash(&self) -> BlockHash {
        self.execution_payload
            .payload_inner
            .payload_inner
            .block_hash
    }

    /// The lean commitment the EVM header commits to (`prev_randao`), if any.
    /// Zero means "no lane" — Arc sets zero when the lane is off.
    pub fn header_lean_commitment(&self) -> Option<BlockHash> {
        let r = self
            .execution_payload
            .payload_inner
            .payload_inner
            .prev_randao;
        (r != BlockHash::ZERO).then_some(r)
    }

    /// True iff the EVM header and the carried lean payload agree: no lean
    /// payload, or `prev_randao == recomputed lean commitment`.
    pub fn lean_binding_ok(&self) -> bool {
        match &self.lean_payload {
            None => true,
            Some(l) => self.header_lean_commitment() == Some(l.commitment()),
        }
    }

    /// Recomputes the canonical block hash from the execution payload contents.
    pub fn canonical_block_hash(&self) -> Result<BlockHash, PayloadError> {
        canonical_block_hash(&self.execution_payload)
    }

    /// Returns whether the self-reported block hash matches the hash recomputed
    /// from the payload contents.
    pub fn self_reported_hash_is_canonical(&self) -> bool {
        self.canonical_block_hash()
            .is_ok_and(|canonical| canonical == self.self_reported_block_hash())
    }

    /// Returns whether this block may be keyed into the undecided-blocks table:
    /// valid blocks always may; invalid blocks only if their self-reported hash
    /// is canonical.
    pub fn may_be_stored_as_undecided(&self) -> bool {
        self.validity.is_valid() || self.self_reported_hash_is_canonical()
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

    /// Builds the [`ProposedValue`] voted on for this block, using `validity` for
    /// the vote instead of the block's persisted [`Self::validity`].
    ///
    /// The value id is always the self-reported (wire) hash peers vote on; only
    /// the vote validity is overridden, so a caller can prevote nil on a block it
    /// still stores with a different (execution-only) validity. A lean payload,
    /// if present, is bound into the EVM header instead (see
    /// [`Self::header_lean_commitment`], [`Self::lean_binding_ok`]) — it never
    /// enters the voted value id.
    pub fn to_proposed_value_with_validity(&self, validity: Validity) -> ProposedValue<ArcContext> {
        ProposedValue {
            height: self.height,
            round: self.round,
            proposer: self.proposer,
            valid_round: self.valid_round,
            value: Value::new(self.self_reported_block_hash()),
            validity,
        }
    }
}

// The value id is the self-reported (wire) hash that peers vote on.
impl From<&ConsensusBlock> for ProposedValue<ArcContext> {
    fn from(block: &ConsensusBlock) -> Self {
        block.to_proposed_value_with_validity(block.validity)
    }
}

impl From<&ConsensusBlock> for LocallyProposedValue<ArcContext> {
    fn from(block: &ConsensusBlock) -> Self {
        LocallyProposedValue {
            height: block.height,
            round: block.round,
            value: Value::new(block.self_reported_block_hash()),
        }
    }
}

/// Recomputes the canonical block hash of an execution payload from its
/// contents.
///
/// Arc has no beacon chain and no execution-layer requests: the
/// `parent_beacon_block_root` is the parent block hash, and the Prague requests
/// list is always empty. The block is reconstructed through the same
/// sidecar-aware path the execution layer uses, so the hash always matches the
/// one the engine validates in `engine_newPayloadV3`.
///
/// Clones the payload to reconstruct the block; keep it off the hot path.
pub fn canonical_block_hash(payload: &ExecutionPayloadV3) -> Result<BlockHash, PayloadError> {
    let parent_beacon_block_root = payload.payload_inner.payload_inner.parent_hash;
    let sidecar = ExecutionPayloadSidecar::v4(
        CancunPayloadFields::new(parent_beacon_block_root, vec![]),
        PraguePayloadFields::new(Requests::default()),
    );
    let block =
        ExecutionData::new(ExecutionPayload::V3(payload.clone()), sidecar).into_block_raw()?;
    Ok(block.header.hash_slow())
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
    )
}

/// Decided block, ie. an execution payload together with its commit certificate.
/// A decided block is a block for which consensus has been reached and therefore
/// it can't be removed from the chain.
#[derive(Clone, Debug)]
pub struct DecidedBlock {
    pub execution_payload: ExecutionPayloadV3,
    pub certificate: CommitCertificate<ArcContext>,
}

impl DecidedBlock {
    /// Creates a new decided block from an execution payload and a commit certificate.
    /// The block hash in the execution payload must match the hash in the commit certificate.
    pub fn new(
        execution_payload: ExecutionPayloadV3,
        certificate: CommitCertificate<ArcContext>,
    ) -> Self {
        let payload_block_hash = execution_payload.payload_inner.payload_inner.block_hash;
        let certificate_block_hash = certificate.value_id.block_hash();

        assert_eq!(
            payload_block_hash, certificate_block_hash,
            "Block hash in the execution payload does not match the hash in the commit certificate"
        );

        Self {
            execution_payload,
            certificate,
        }
    }

    /// Returns the height at which the block was decided.
    pub fn height(&self) -> Height {
        self.certificate.height
    }
}

// ---------------------------------------------------------------------------
// Lane framing — proposal parts and value-sync frames.
//
//   [u64 LE: len(evm SSZ) | LEAN_LANE_BIT?] [evm SSZ] [lean block bytes?]
//
// Single-lane frames carry the plain length; a lean frame sets LEAN_LANE_BIT
// and appends the lean block bytes verbatim (the lean node's canonical wire
// form, decoded and commitment-checked on receipt). Any other high bit is an
// unknown format and fails closed.
// ---------------------------------------------------------------------------

/// Flag in the length prefix marking a lean-lane frame.
pub const LEAN_LANE_BIT: u64 = 1 << 62;

/// Upper bound on transactions in one lean block (decode-time DoS guard).
pub const MAX_LEAN_TXS: usize = 200_000;

/// Bits of the length prefix that may legitimately be set: the length itself
/// (well under 2^40 bytes) and the lane flag.
const LANE_LEN_MASK: u64 = (1u64 << 40) - 1;

/// Decoded view of a lean block's header, with the commitment recomputed from
/// the bytes (never trusted from the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeanBlockRef {
    pub parent: BlockHash,
    pub number: u64,
    pub timestamp_ms: u64,
    pub tx_count: u32,
    pub commitment: BlockHash,
}

/// Fixed-width little-endian read at `off`, without a fallible `try_into` (and
/// so without an `unwrap` that a future layout change could turn into a panic
/// on attacker-supplied bytes): a slice too short to hold the field reads as a
/// decode error, which is what every caller here wants.
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

/// Strict decode of the lean node's canonical block bytes:
/// `[parent 32][number u64 LE][timestamp_ms u64 LE][n u32 LE]([len u32 LE][tx])*`,
/// recomputing `commitment = keccak(parent ‖ number ‖ timestamp_ms ‖ keccak(framed tx section))`.
pub fn decode_lean_block(bytes: &[u8]) -> eyre::Result<LeanBlockRef> {
    const HDR: usize = 32 + 8 + 8 + 4;
    if bytes.len() < HDR {
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
    let mut off = HDR;
    for i in 0..tx_count {
        let len_end = off
            .checked_add(4)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| eyre::eyre!("lean block truncated at tx {i} length"))?;
        let len = le_u32_at(bytes, off)? as usize;
        let tx_end = len_end
            .checked_add(len)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| eyre::eyre!("lean block truncated at tx {i} body (len {len})"))?;
        off = tx_end;
    }
    if off != bytes.len() {
        return Err(eyre::eyre!(
            "lean block has {} trailing bytes after {tx_count} txs",
            bytes.len().saturating_sub(off)
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

/// Frame the lanes of a block for the wire. `lean_bytes = None` produces a
/// single-lane frame.
pub fn frame_lanes(execution_payload: &ExecutionPayloadV3, lean_bytes: Option<&[u8]>) -> Vec<u8> {
    let evm = execution_payload.as_ssz_bytes();
    let lean_len = lean_bytes.map_or(0, <[u8]>::len);
    let mut buf = Vec::with_capacity(8usize.saturating_add(evm.len()).saturating_add(lean_len));
    let mut prefix = evm.len() as u64;
    if lean_bytes.is_some() {
        prefix |= LEAN_LANE_BIT;
    }
    buf.extend_from_slice(&prefix.to_le_bytes());
    buf.extend_from_slice(&evm);
    if let Some(lean) = lean_bytes {
        buf.extend_from_slice(lean);
    }
    buf
}

/// Encode a consensus value for the wire.
///
/// `lean_lane == false` (the default, `ARC_PAYMENT_LEAN_LANE` unset): the value
/// is the EVM payload's SSZ bytes and nothing else — the stock format, byte for
/// byte. `lean_lane == true`: every value carries the lane frame (see
/// [`frame_lanes`]), whether or not this height has a lean block, so a
/// lane-enabled fleet has ONE wire format across activation. A lean payload is
/// always framed, regardless of the flag, so it can never be silently dropped.
pub fn encode_value(
    execution_payload: &ExecutionPayloadV3,
    lean_bytes: Option<&[u8]>,
    lean_lane: bool,
) -> Vec<u8> {
    if lean_lane || lean_bytes.is_some() {
        frame_lanes(execution_payload, lean_bytes)
    } else {
        execution_payload.as_ssz_bytes()
    }
}

/// Decode a consensus value from the wire; the inverse of [`encode_value`].
/// The format is selected by the node's own flag, never sniffed from the
/// bytes: a mixed-flag fleet fails closed with a decode error.
pub fn decode_value(bytes: &[u8], lean_lane: bool) -> eyre::Result<LaneFrame> {
    if lean_lane {
        unframe_lanes(bytes)
    } else {
        ExecutionPayloadV3::from_ssz_bytes(bytes)
            .map(LaneFrame::Evm)
            .map_err(|e| eyre::eyre!("failed to SSZ-decode execution payload: {e:?}"))
    }
}

/// A decoded lane frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneFrame {
    /// Single-lane block.
    Evm(ExecutionPayloadV3),
    /// EVM lane plus the lean lane as canonical block bytes. `lean` is the
    /// decoded+verified view; the raw bytes are kept verbatim for the
    /// `arc_newBlock` feed and for re-framing.
    LeanPayment {
        execution_payload: ExecutionPayloadV3,
        lean: LeanBlockRef,
        lean_bytes: Vec<u8>,
    },
}

impl LaneFrame {
    /// Split into the EVM payload and the (optional) lean payload.
    pub fn into_parts(self) -> (ExecutionPayloadV3, Option<LeanLanePayload>) {
        match self {
            LaneFrame::Evm(evm) => (evm, None),
            LaneFrame::LeanPayment {
                execution_payload,
                lean,
                lean_bytes,
            } => (
                execution_payload,
                Some(LeanLanePayload {
                    decoded: lean,
                    bytes: lean_bytes,
                }),
            ),
        }
    }
}

/// Decode a lane frame. Fails closed on unknown format flags.
pub fn unframe_lanes(bytes: &[u8]) -> eyre::Result<LaneFrame> {
    if bytes.len() < 8 {
        return Err(eyre::eyre!("lane bytes too short to contain length prefix"));
    }
    let raw = le_u64_at(bytes, 0)?;
    let flags = raw & !LANE_LEN_MASK;
    if flags & !LEAN_LANE_BIT != 0 {
        return Err(eyre::eyre!(
            "unknown lane frame flags {flags:#x} — a newer wire format (mixed-version fleet?)"
        ));
    }
    let len_evm = (raw & LANE_LEN_MASK) as usize;
    let evm_end = 8usize
        .checked_add(len_evm)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| eyre::eyre!("invalid evm payload length prefix"))?;
    let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&bytes[8..evm_end])
        .map_err(|e| eyre::eyre!("Failed to decode execution payload: {e:?}"))?;
    if flags & LEAN_LANE_BIT != 0 {
        let lean_bytes = bytes[evm_end..].to_vec();
        let lean = decode_lean_block(&lean_bytes)?;
        return Ok(LaneFrame::LeanPayment {
            execution_payload,
            lean,
            lean_bytes,
        });
    }
    if bytes.len() != evm_end {
        return Err(eyre::eyre!(
            "single-lane frame has {} trailing bytes",
            bytes.len().saturating_sub(evm_end)
        ));
    }
    Ok(LaneFrame::Evm(execution_payload))
}

/// Canonical lean block bytes with no transactions (layout:
/// docs/lean-lane-integration.md §2):
/// `[parent 32][number u64 LE][timestamp_ms u64 LE][n=0 u32 LE]`. Public and
/// doc-hidden (not `#[cfg(test)]`) so both this crate's tests and
/// `arc-node-consensus`'s can build lean bytes identically without
/// duplicating the layout.
#[doc(hidden)]
pub fn test_lean_block_bytes(parent: BlockHash, number: u64, timestamp_ms: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(52);
    b.extend_from_slice(parent.as_slice());
    b.extend_from_slice(&number.to_le_bytes());
    b.extend_from_slice(&timestamp_ms.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, B256};

    fn block_with_self_reported_hash(self_reported: BlockHash) -> ConsensusBlock {
        let inner: Block = Block::default();
        let mut payload = ExecutionPayloadV3::from_block_unchecked(self_reported, &inner);
        payload.payload_inner.payload_inner.block_hash = self_reported;

        ConsensusBlock {
            height: Height::new(1),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: Address::new([0u8; 20]),
            validity: Validity::Valid,
            execution_payload: payload,
            signature: None,
            lean_payload: None,
        }
    }

    #[test]
    fn canonical_block_hash_is_independent_of_self_reported_hash() {
        let canonical = block_with_self_reported_hash(B256::ZERO)
            .canonical_block_hash()
            .expect("recompute canonical hash");

        // The canonical hash is derived from the payload contents, so it is the
        // same regardless of which value the payload claims as its block hash.
        let other = block_with_self_reported_hash(B256::repeat_byte(0xAB))
            .canonical_block_hash()
            .expect("recompute canonical hash");

        assert_eq!(canonical, other);
    }

    #[test]
    fn self_reported_hash_is_canonical_only_when_it_matches_contents() {
        let canonical = block_with_self_reported_hash(B256::ZERO)
            .canonical_block_hash()
            .expect("recompute canonical hash");

        let matching = block_with_self_reported_hash(canonical);
        assert_eq!(matching.self_reported_block_hash(), canonical);
        assert!(matching.self_reported_hash_is_canonical());

        let mismatched = block_with_self_reported_hash(B256::repeat_byte(0x99));
        assert!(!mismatched.self_reported_hash_is_canonical());
        assert_eq!(
            mismatched.canonical_block_hash().expect("recompute"),
            canonical
        );
    }

    #[test]
    fn may_be_stored_as_undecided_rejects_only_invalid_non_canonical_blocks() {
        let canonical = block_with_self_reported_hash(B256::ZERO)
            .canonical_block_hash()
            .expect("recompute canonical hash");

        let mut valid_non_canonical = block_with_self_reported_hash(B256::repeat_byte(0x99));
        valid_non_canonical.validity = Validity::Valid;
        assert!(valid_non_canonical.may_be_stored_as_undecided());

        let mut invalid_canonical = block_with_self_reported_hash(canonical);
        invalid_canonical.validity = Validity::Invalid;
        assert!(invalid_canonical.may_be_stored_as_undecided());

        let mut invalid_non_canonical = block_with_self_reported_hash(B256::repeat_byte(0x99));
        invalid_non_canonical.validity = Validity::Invalid;
        assert!(!invalid_non_canonical.may_be_stored_as_undecided());
    }

    #[test]
    fn to_proposed_value_with_validity_overrides_only_the_vote_validity() {
        let mut block = block_with_self_reported_hash(B256::repeat_byte(0x7));
        block.validity = Validity::Valid;

        let voted = block.to_proposed_value_with_validity(Validity::Invalid);

        // The vote validity is overridden, while the block's persisted validity
        // is untouched and the value id + metadata still match the block.
        assert_eq!(voted.validity, Validity::Invalid);
        assert_eq!(block.validity, Validity::Valid);
        assert_eq!(voted.value, Value::new(block.self_reported_block_hash()));
        assert_eq!(voted.height, block.height);
        assert_eq!(voted.round, block.round);
        assert_eq!(voted.proposer, block.proposer);
        assert_eq!(voted.valid_round, block.valid_round);

        // The `From` impl keeps deriving the vote validity from the block.
        assert_eq!(ProposedValue::from(&block).validity, Validity::Valid);
    }

    #[test]
    fn canonical_block_hash_includes_prague_requests_hash() {
        // Arc runs Prague with no execution requests, so the execution layer
        // seals headers with the empty-requests hash and the parent hash as the
        // beacon root. The canonical hash must match that reconstruction, not
        // the requests_hash = None one that `into_block_raw` produces on its own.
        let payload = block_with_self_reported_hash(B256::ZERO).execution_payload;

        let mut expected = payload.clone().into_block_raw().expect("into_block_raw");
        expected.header.parent_beacon_block_root =
            Some(payload.payload_inner.payload_inner.parent_hash);
        expected.header.requests_hash = Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH);

        assert_eq!(
            canonical_block_hash(&payload).expect("canonical hash"),
            expected.header.hash_slow(),
        );
    }
}

#[cfg(test)]
mod lane_tests {
    use super::*;
    use crate::{Address, Height, Round};
    use alloy_primitives::{Bloom, Bytes as AlloyBytes, B256, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2};
    use malachitebft_app_channel::app::types::core::Validity;

    /// Builds a payload whose block hash (and state root) is keyed by `seed`.
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

    fn block(evm: ExecutionPayloadV3, lean: Option<LeanLanePayload>) -> ConsensusBlock {
        ConsensusBlock {
            height: Height::new(1),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: Address::new([0u8; 20]),
            validity: Validity::Valid,
            execution_payload: evm,
            signature: None,
            lean_payload: lean,
        }
    }

    /// A block carrying a lean payload of `n_txs` tiny transactions.
    fn block_with_lean(seed: u8, n_txs: usize) -> ConsensusBlock {
        let txs: Vec<Vec<u8>> = (0..n_txs)
            .map(|i| vec![seed, u8::try_from(i % 256).unwrap()])
            .collect();
        let tx_refs: Vec<&[u8]> = txs.iter().map(Vec::as_slice).collect();
        block(payload(seed), Some(lean(seed, 1, 1000, &tx_refs)))
    }

    /// A single-lane block with no lean payload.
    fn block_without_lean(seed: u8) -> ConsensusBlock {
        block(payload(seed), None)
    }

    /// Builds canonical lean block bytes for tests (mirrors the lean-lane-node
    /// encoding: [parent][number LE][ts_ms LE][n u32 LE]([len u32 LE][tx])*).
    fn lean_bytes(parent: u8, number: u64, ts_ms: u64, txs: &[&[u8]]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(B256::repeat_byte(parent).as_slice());
        b.extend_from_slice(&number.to_le_bytes());
        b.extend_from_slice(&ts_ms.to_le_bytes());
        b.extend_from_slice(&u32::try_from(txs.len()).unwrap().to_le_bytes());
        for t in txs {
            b.extend_from_slice(&u32::try_from(t.len()).unwrap().to_le_bytes());
            b.extend_from_slice(t);
        }
        b
    }

    fn lean(parent: u8, number: u64, ts_ms: u64, txs: &[&[u8]]) -> LeanLanePayload {
        LeanLanePayload::new(lean_bytes(parent, number, ts_ms, txs)).unwrap()
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
            assert_ne!(
                decode_lean_block(&variant).unwrap().commitment,
                a.commitment
            );
        }
        // SECURITY: boundary shifts with identical concatenated bodies MUST
        // move the commitment — otherwise two framings of the same bytes share
        // a commitment but decode to different tx lists, and total-STF executes
        // both (same commitment, divergent state; exploitable via sync).
        let b = decode_lean_block(&lean_bytes(0xAA, 7, 1000, &[b"tx-onetx-two"])).unwrap();
        assert_ne!(
            b.commitment, a.commitment,
            "commitment must bind tx FRAMING, not just bodies"
        );
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
        let n = (u32::try_from(MAX_LEAN_TXS).unwrap() + 1).to_le_bytes();
        c[48..52].copy_from_slice(&n);
        assert!(decode_lean_block(&c).is_err());
        // claimed count larger than actual bodies
        let mut d = lean_bytes(0xAA, 1, 1, &[b"abc"]);
        d[48..52].copy_from_slice(&2u32.to_le_bytes());
        assert!(decode_lean_block(&d).is_err());
    }

    #[test]
    fn lean_frame_round_trips_and_unknown_flags_fail_closed() {
        let evm = payload(0x11);
        let lb = lean_bytes(0xAA, 3, 500, &[b"tx-a", b"tx-b", b"tx-c"]);
        let framed = frame_lanes(&evm, Some(&lb));
        match unframe_lanes(&framed).unwrap() {
            LaneFrame::LeanPayment {
                execution_payload,
                lean,
                lean_bytes,
            } => {
                assert_eq!(execution_payload, evm);
                assert_eq!(lean_bytes, lb);
                assert_eq!(lean, decode_lean_block(&lb).unwrap());
            }
            other => panic!("expected LeanPayment, got {other:?}"),
        }
        // An unknown format bit (a newer wire format) must fail closed.
        let mut unknown = framed.clone();
        let raw = u64::from_le_bytes(unknown[..8].try_into().unwrap()) | (1 << 63);
        unknown[..8].copy_from_slice(&raw.to_le_bytes());
        assert!(unframe_lanes(&unknown).is_err());
        // into_parts hands the lean payload back validated.
        let (e, l) = unframe_lanes(&framed).unwrap().into_parts();
        assert_eq!(e, evm);
        assert_eq!(l.unwrap().bytes, lb);
    }

    /// CROSS-IMPLEMENTATION PIN: exact blockBytes + commitments produced by the
    /// lean lane node's `gen_vector` example (chain 1338). If either side
    /// changes the wire format or commitment formula, this breaks FIRST.
    /// Genesis = keccak("ARC_LEAN_LANE_GENESIS" ‖ 1338 BE).
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

    #[test]
    fn frame_unframe_round_trips_single_lane() {
        let evm = payload(0x11);
        let framed = frame_lanes(&evm, None);
        assert!(matches!(unframe_lanes(&framed).unwrap(), LaneFrame::Evm(e) if e == evm));
        // Trailing bytes on a single-lane frame are an error, not silently
        // ignored lane data.
        let mut trailing = framed.clone();
        trailing.push(0);
        assert!(unframe_lanes(&trailing).is_err());
    }

    /// Flag-off framing: a single-lane frame's prefix is exactly the payload
    /// length — no lane bits — so every single-lane node frames identically.
    #[test]
    fn single_lane_frame_prefix_is_plain_length() {
        let evm = payload(0x11);
        let framed = frame_lanes(&evm, None);
        let prefix = u64::from_le_bytes(framed[..8].try_into().unwrap());
        assert_eq!(prefix, evm.as_ssz_bytes().len() as u64);
        assert_eq!(prefix & !LANE_LEN_MASK, 0);
    }

    #[test]
    fn encode_value_flag_off_is_stock_ssz() {
        let evm = payload(0x11);
        let bytes = encode_value(&evm, None, false);
        assert_eq!(
            bytes,
            evm.as_ssz_bytes(),
            "flag off must be the stock wire format"
        );
        assert_eq!(decode_value(&bytes, false).unwrap(), LaneFrame::Evm(evm));
    }

    #[test]
    fn encode_value_flag_on_frames_evm_only_heights() {
        let evm = payload(0x11);
        let bytes = encode_value(&evm, None, true);
        assert_eq!(bytes, frame_lanes(&evm, None));
        assert_eq!(decode_value(&bytes, true).unwrap(), LaneFrame::Evm(evm));
    }

    #[test]
    fn decode_value_flag_on_rejects_stock_ssz() {
        // parent_hash = 0x11.. reads as a prefix with flag bits outside LEAN_LANE_BIT
        let evm = payload(0x11);
        assert!(decode_value(&evm.as_ssz_bytes(), true).is_err());
    }

    #[test]
    fn encode_value_never_drops_a_lean_payload() {
        let evm = payload(0x11);
        let bytes = encode_value(&evm, Some(b"x"), false);
        assert_eq!(bytes, frame_lanes(&evm, Some(b"x")));
    }

    #[test]
    fn binding_ok_when_header_carries_the_lean_commitment() {
        let mut b = block_with_lean(0x11, 3);
        let c = b.lean_payload.as_ref().unwrap().commitment();
        b.execution_payload.payload_inner.payload_inner.prev_randao = c;
        assert!(b.lean_binding_ok());
        assert_eq!(b.header_lean_commitment(), Some(c));
    }

    #[test]
    fn binding_fails_when_header_disagrees() {
        let mut b = block_with_lean(0x11, 3);
        b.execution_payload.payload_inner.payload_inner.prev_randao = B256::repeat_byte(0xee);
        assert!(!b.lean_binding_ok());
    }

    /// The exact safety case: lean bytes carried, header left at zero. The
    /// bytes are then bound to nothing the certificate covers — a proposer
    /// could swap them freely — so the block must not read as bound.
    #[test]
    fn binding_fails_when_a_lean_block_is_carried_under_a_zero_header() {
        let b = block_with_lean(0x11, 3);
        assert_eq!(
            b.execution_payload.payload_inner.payload_inner.prev_randao,
            B256::ZERO,
            "the helper leaves prev_randao unset",
        );
        assert_eq!(b.header_lean_commitment(), None);
        assert!(
            !b.lean_binding_ok(),
            "lean bytes under a zero prev_randao are not committed to by anything"
        );
    }

    #[test]
    fn no_lean_payload_is_always_bound_and_zero_header_means_no_lane() {
        let b = block_without_lean(0x11);
        assert!(b.lean_binding_ok());
        assert_eq!(b.header_lean_commitment(), None);
    }
}
