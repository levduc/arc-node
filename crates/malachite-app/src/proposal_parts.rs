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

use bytes::Bytes;
use eyre::Context as _;
use sha3::Digest;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::streaming::CHUNK_SIZE;

use malachitebft_app_channel::app::streaming::{StreamContent, StreamId, StreamMessage};
use malachitebft_app_channel::app::types::core::{Round, Validity};
use malachitebft_app_channel::NetworkMsg;

use arc_consensus_types::proposer::ProposerSelector;
use arc_consensus_types::signing::{Signature, SigningError, SigningProvider, VerificationResult};
use arc_consensus_types::{
    ArcContext, Height, ProposalData, ProposalFin, ProposalInit, ProposalPart, ProposalParts,
    Validator, ValidatorSet,
};

use crate::block::{frame_lanes, unframe_lanes, ConsensusBlock};
use arc_consensus_types::block::{unframe_lanes_any, LaneFrame};
use arc_eth_engine::engine::Engine;

#[cfg_attr(test, mockall::automock(type Error = std::io::Error;))]
pub trait PublishProposalPart {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn publish_proposal_part(
        &self,
        msg: StreamMessage<ProposalPart>,
    ) -> Result<(), Self::Error>;
}

impl<T> PublishProposalPart for &'_ T
where
    T: PublishProposalPart,
{
    type Error = T::Error;

    async fn publish_proposal_part(
        &self,
        msg: StreamMessage<ProposalPart>,
    ) -> Result<(), Self::Error> {
        (*self).publish_proposal_part(msg).await
    }
}

impl PublishProposalPart for mpsc::Sender<NetworkMsg<ArcContext>> {
    type Error = mpsc::error::SendError<NetworkMsg<ArcContext>>;

    async fn publish_proposal_part(
        &self,
        msg: StreamMessage<ProposalPart>,
    ) -> Result<(), Self::Error> {
        self.send(NetworkMsg::PublishProposalPart(msg)).await
    }
}

/// Streams the given proposal parts over the network.
pub async fn stream_proposal(
    publish: impl PublishProposalPart,
    height: Height,
    round: Round,
    stream_messages: Vec<StreamMessage<ProposalPart>>,
) -> Result<(), eyre::Error> {
    for msg in stream_messages {
        info!(
            %height, %round, stream_id = %msg.stream_id, sequence = %msg.sequence,
            "Streaming proposal part: {:?}", msg.content
        );

        publish
            .publish_proposal_part(msg)
            .await
            .wrap_err("Failed to send proposal part to network")?;
    }

    Ok(())
}

/// Splits the given consensus block into proposal parts and prepares stream messages
/// for each part, along with the signature of the entire proposal.
pub async fn prepare_stream(
    stream_id: StreamId,
    signing_provider: &impl SigningProvider<ArcContext>,
    consensus_block: &ConsensusBlock,
) -> eyre::Result<(Vec<StreamMessage<ProposalPart>>, Signature)> {
    let (parts, signature) = make_proposal_parts(signing_provider, consensus_block)
        .await
        .wrap_err("Failed to construct proposal parts")?;

    // +1 for the Fin message; Vec length <= isize::MAX, so +1 cannot overflow usize
    #[allow(clippy::arithmetic_side_effects)]
    let mut msgs = Vec::with_capacity(parts.len() + 1);
    let mut sequence = 0u64;

    for part in parts {
        let msg = StreamMessage::new(stream_id.clone(), sequence, StreamContent::Data(part));
        // Bounded by parts.len() which is bounded by MAX_MESSAGES_PER_STREAM
        #[allow(clippy::arithmetic_side_effects)]
        {
            sequence += 1;
        }
        msgs.push(msg);
    }

    msgs.push(StreamMessage::new(stream_id, sequence, StreamContent::Fin));

    Ok((msgs, signature))
}

/// Splits the given consensus block into proposal parts and computes the signature
/// for the entire proposal.
pub async fn make_proposal_parts(
    signing_provider: &impl SigningProvider<ArcContext>,
    block: &ConsensusBlock,
) -> Result<(Vec<ProposalPart>, Signature), SigningError> {
    let mut hasher = sha3::Keccak256::new();
    let mut parts = Vec::new();

    // Framed payload bytes: [u64-LE len(evm)] [evm SSZ] [payment SSZ (optional)].
    // The length prefix lets the decoder split the two lanes; absent payment lane => no trailer.
    let data = frame_lanes(&block.execution_payload, block.payment_payload.as_ref());

    // Init
    {
        parts.push(ProposalPart::Init(ProposalInit::new(
            block.height,
            block.round,
            block.valid_round,
            block.proposer,
        )));

        hasher.update(block.height.as_u64().to_be_bytes().as_slice());
        hasher.update(block.round.as_i64().to_be_bytes().as_slice());
    }

    // Data
    {
        for chunk in data.chunks(CHUNK_SIZE) {
            let chunk_data = ProposalData::new(Bytes::copy_from_slice(chunk));
            parts.push(ProposalPart::Data(chunk_data));
            hasher.update(chunk);
        }
    }

    // Fin
    let signature = match &block.signature {
        Some(signature) => {
            // Use the existing signature if it exists (restreaming)
            *signature
        }
        None => {
            // We are streaming a new proposal, so we need to sign it
            let hash = hasher.finalize().to_vec();
            signing_provider.sign_bytes(&hash).await?
        }
    };

    parts.push(ProposalPart::Fin(ProposalFin::new(signature)));

    Ok((parts, signature))
}

/// Validates the proposal parts by checking the proposer and signature.
///
/// ## Important
/// This function assumes that the parts are for the current height
pub async fn validate_proposal_parts(
    parts: &ProposalParts,
    expected_proposer: &Validator,
    signing_provider: &impl SigningProvider<ArcContext>,
) -> bool {
    // Check that the parts are from the expected proposer
    if expected_proposer.address != parts.proposer() {
        warn!(
            parts.height = %parts.height(),
            parts.round = %parts.round(),
            parts.proposer = %parts.proposer(),
            expected_proposer = %expected_proposer.address,
            "Received proposal part from non-proposer, ignoring"
        );

        return false;
    }

    let fin = parts.fin();
    let hash = parts.hash();

    assert_eq!(
        expected_proposer.address,
        parts.proposer(),
        "Proposer address must match expected proposer"
    );

    // Check proposal parts signature
    // NOTE: `expected_proposer` is guaranteed to be the proposer of these parts
    let result = signing_provider
        .verify_signed_bytes(&hash, &fin.signature, &expected_proposer.public_key)
        .await;

    match result {
        Ok(VerificationResult::Valid) => true,

        Ok(VerificationResult::Invalid) => {
            warn!(
                parts.height = %parts.height(),
                parts.round = %parts.round(),
                parts.proposer = %parts.proposer(),
                parts.hash = %hex::encode(hash),
                parts.signature = %hex::encode(fin.signature.to_bytes()),
                "Received proposal parts with invalid signature, ignoring"
            );

            false
        }

        Err(error) => {
            warn!(
                parts.height = %parts.height(),
                parts.round = %parts.round(),
                parts.proposer = %parts.proposer(),
                parts.hash = %hex::encode(hash),
                parts.signature = %hex::encode(fin.signature.to_bytes()),
                %error,
                "Error verifying proposal parts signature, ignoring"
            );

            false
        }
    }
}

/// Resolves the expected proposer for a set of proposal parts.
///
/// When `pol_round` (proof-of-lock round) is set, the proposal is a re-stream
/// of a locked block. The proposer embedded in the parts is the original proposer
/// from `pol_round`, not the proposer for `parts.round()` (the restream round).
pub fn resolve_expected_proposer<'a>(
    proposer_selector: &dyn ProposerSelector,
    validator_set: &'a ValidatorSet,
    parts: &ProposalParts,
) -> &'a Validator {
    let pol_round = parts.init().pol_round;
    let proposer_round = if pol_round != Round::Nil {
        pol_round
    } else {
        parts.round()
    };
    proposer_selector.select_proposer(validator_set, parts.height(), proposer_round)
}

/// Re-assemble a [`ConsensusBlock`] from its [`ProposalParts`].
///
/// Handles BOTH wire formats unconditionally (full and compact — mixed
/// new-binary fleets interoperate regardless of per-node emission flags).
/// Compact frames need the payment EL to reconstruct the tx list, hence the
/// async signature and the engine handle; callers on full-only paths may pass
/// `None` (a compact frame then errors out loudly).
pub async fn assemble_block_from_parts(
    parts: &ProposalParts,
    payment_engine: Option<&Engine>,
) -> eyre::Result<ConsensusBlock> {
    // Calculate total size and allocate buffer
    let total_size = parts.data_size();
    let mut block_bytes = Vec::with_capacity(total_size);

    // Concatenate all chunks
    for part in parts.data() {
        block_bytes.extend_from_slice(&part.bytes);
    }

    let (execution_payload, payment_payload) = match unframe_lanes_any(&block_bytes)? {
        LaneFrame::Full(evm, pay) => (evm, pay),
        LaneFrame::CompactPayment {
            execution_payload,
            payment_header,
            tx_hashes,
        } => {
            let engine = payment_engine.ok_or_else(|| {
                eyre::eyre!(
                    "compact payment proposal received but no payment engine available \
                     to reconstruct it"
                )
            })?;
            let payment =
                reconstruct_compact_payment(engine, payment_header, &tx_hashes).await?;
            (execution_payload, Some(payment))
        }
    };

    let consensus_block = ConsensusBlock {
        height: parts.height(),
        round: parts.round(),
        valid_round: parts.init().pol_round,
        proposer: parts.proposer(),
        validity: Validity::Valid,
        execution_payload,
        signature: Some(parts.fin().signature),
        payment_payload,
    };

    Ok(consensus_block)
}

/// Rebuilds a full payment payload from a compact frame: batch-fetch the raw
/// tx bytes from the local payment EL (pool first, then DB), verify each
/// fetched tx hashes to the requested hash (guards EL bugs — µs-cheap), and
/// re-attach the tx list to the header. Any miss is an assembly failure: the
/// caller's existing failure path yields no proposed value, the round times
/// out and rotates. Downstream, the structural/newPayload block-hash check
/// verifies the reconstruction byte-exactly (tx root → header hash), so a
/// wrong reconstruction can never be voted Valid.
async fn reconstruct_compact_payment(
    payment_engine: &Engine,
    mut payment_header: alloy_rpc_types_engine::ExecutionPayloadV3,
    tx_hashes: &[arc_consensus_types::BlockHash],
) -> eyre::Result<alloy_rpc_types_engine::ExecutionPayloadV3> {
    use sha3::{Digest, Keccak256};

    let fetched = payment_engine
        .eth
        .get_raw_transactions_by_hash(tx_hashes)
        .await
        .wrap_err("compact reconstruction: raw-tx batch fetch failed")?;
    if fetched.len() != tx_hashes.len() {
        return Err(eyre::eyre!(
            "compact reconstruction: EL returned {} entries for {} hashes",
            fetched.len(),
            tx_hashes.len()
        ));
    }

    let mut txs = Vec::with_capacity(tx_hashes.len());
    let mut missing = 0usize;
    for (i, (raw, want)) in fetched.into_iter().zip(tx_hashes).enumerate() {
        match raw {
            Some(bytes) => {
                let mut hasher = Keccak256::new();
                hasher.update(bytes.as_ref());
                if hasher.finalize().as_slice() != want.as_slice() {
                    return Err(eyre::eyre!(
                        "compact reconstruction: EL returned bytes not matching hash {want} at index {i}"
                    ));
                }
                txs.push(bytes);
            }
            None => missing += 1,
        }
    }
    if missing > 0 {
        return Err(eyre::eyre!(
            "compact reconstruction: {missing}/{} txs not found in local payment EL",
            tx_hashes.len()
        ));
    }

    payment_header.payload_inner.payload_inner.transactions = txs;
    Ok(payment_header)
}

#[cfg(test)]
mod tests {
    use super::*;

    use arc_consensus_types::proposer::RoundRobin;
    use arc_consensus_types::signing::SigningProvider;
    use arc_consensus_types::{Address, ProposalFin, ProposalInit, ValidatorSet};
    use arc_signer::local::{LocalSigningProvider, PrivateKey, PublicKey};

    use arc_eth_engine::engine::{MockEngineAPI, MockEthereumAPI};
    use sha3::{Digest as _, Keccak256};

    fn engine_with_pool(pool: Vec<alloy_primitives::Bytes>) -> Engine {
        let mut eth = MockEthereumAPI::new();
        eth.expect_get_raw_transactions_by_hash().returning(move |hashes| {
            let pool = pool.clone();
            let out = hashes
                .iter()
                .map(|want| {
                    pool.iter()
                        .find(|b| {
                            let mut h = Keccak256::new();
                            h.update(b.as_ref());
                            h.finalize().as_slice() == want.as_slice()
                        })
                        .cloned()
                })
                .collect();
            Ok(out)
        });
        Engine::new(Box::new(MockEngineAPI::new()), Box::new(eth))
    }

    #[tokio::test]
    async fn compact_reconstruction_round_trip_and_misses() {
        use arc_consensus_types::block::{frame_lanes_compact, unframe_lanes_any, LaneFrame};

        let txs = vec![
            alloy_primitives::Bytes::from(vec![0x02, 0xaa, 0xbb]),
            alloy_primitives::Bytes::from(vec![0x02, 0xcc]),
        ];
        let evm = crate::block::tests_payload_helper(0x10, vec![]);
        let pay = crate::block::tests_payload_helper(0x20, txs.clone());
        let framed = frame_lanes_compact(&evm, &pay);
        let LaneFrame::CompactPayment { payment_header, tx_hashes, .. } =
            unframe_lanes_any(&framed).unwrap()
        else {
            panic!("expected compact frame")
        };

        // happy path: full pool -> byte-identical payload
        let engine = engine_with_pool(txs.clone());
        let rebuilt = reconstruct_compact_payment(&engine, payment_header.clone(), &tx_hashes)
            .await
            .unwrap();
        assert_eq!(rebuilt, pay);

        // missing tx -> loud error naming the miss count
        let engine = engine_with_pool(txs[..1].to_vec());
        let err = reconstruct_compact_payment(&engine, payment_header, &tx_hashes)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("1/2"), "err: {err:#}");
    }

    fn make_validator_set(n: usize) -> (Vec<PrivateKey>, ValidatorSet) {
        let mut rng = rand::thread_rng();
        let keys: Vec<PrivateKey> = (0..n).map(|_| PrivateKey::generate(&mut rng)).collect();
        let validators: Vec<Validator> = keys
            .iter()
            .map(|k| Validator::new(k.public_key(), 1))
            .collect();
        (keys, ValidatorSet::new(validators))
    }

    /// Build minimal ProposalParts with the given init fields and sign with the given key.
    async fn make_signed_parts(
        height: Height,
        round: Round,
        pol_round: Round,
        proposer_pub: PublicKey,
        signing_key: &PrivateKey,
    ) -> ProposalParts {
        use sha3::Digest;

        let proposer = Address::from_public_key(&proposer_pub);
        let init = ProposalInit::new(height, round, pol_round, proposer);

        let mut hasher = sha3::Keccak256::new();
        hasher.update(height.as_u64().to_be_bytes());
        hasher.update(round.as_i64().to_be_bytes());
        let hash = hasher.finalize().to_vec();

        let provider = LocalSigningProvider::new(signing_key.clone());
        let signature = provider.sign_bytes(&hash).await.unwrap();

        ProposalParts::new(vec![
            ProposalPart::Init(init),
            ProposalPart::Fin(ProposalFin::new(signature)),
        ])
        .unwrap()
    }

    #[test]
    fn resolve_proposer_without_pol_round_uses_parts_round() {
        let selector = RoundRobin;
        let (_keys, validator_set) = make_validator_set(3);

        let height = Height::new(1);
        let round = Round::new(2);

        // Build minimal parts with pol_round = Nil
        let init = ProposalInit::new(
            height,
            round,
            Round::Nil,
            validator_set.get_by_index(0).unwrap().address,
        );
        let fin = ProposalFin::new(arc_consensus_types::signing::Signature::test());
        let parts =
            ProposalParts::new(vec![ProposalPart::Init(init), ProposalPart::Fin(fin)]).unwrap();

        let expected = resolve_expected_proposer(&selector, &validator_set, &parts);
        let round_proposer = selector.select_proposer(&validator_set, height, round);

        assert_eq!(expected.address, round_proposer.address);
    }

    #[test]
    fn resolve_proposer_with_pol_round_uses_original_round() {
        let selector = RoundRobin;
        let (_keys, validator_set) = make_validator_set(3);

        let height = Height::new(1);
        let restream_round = Round::new(2);
        let pol_round = Round::new(0);

        let original_proposer = selector.select_proposer(&validator_set, height, pol_round);
        let restream_proposer = selector.select_proposer(&validator_set, height, restream_round);

        // Ensure they differ so the test is meaningful
        assert_ne!(
            original_proposer.address, restream_proposer.address,
            "Test requires different proposers for pol_round and restream_round"
        );

        // Build parts as if restreamed: round=2, pol_round=0, proposer=original
        let init = ProposalInit::new(height, restream_round, pol_round, original_proposer.address);
        let fin = ProposalFin::new(arc_consensus_types::signing::Signature::test());
        let parts =
            ProposalParts::new(vec![ProposalPart::Init(init), ProposalPart::Fin(fin)]).unwrap();

        let expected = resolve_expected_proposer(&selector, &validator_set, &parts);

        // Should resolve to the pol_round proposer, not the restream round proposer
        assert_eq!(expected.address, original_proposer.address);
        assert_ne!(expected.address, restream_proposer.address);
    }

    /// End-to-end: restreamed proposal parts signed by the original proposer
    /// pass validation when expected_proposer is resolved via pol_round.
    #[tokio::test]
    async fn restreamed_parts_pass_validation_with_pol_round_proposer() {
        let selector = RoundRobin;
        let (keys, validator_set) = make_validator_set(3);

        let height = Height::new(1);
        let pol_round = Round::new(0);
        let restream_round = Round::new(2);

        let original_proposer = selector.select_proposer(&validator_set, height, pol_round);

        // Find the signing key for the original proposer
        let signing_key = keys
            .iter()
            .find(|k| Address::from_public_key(&k.public_key()) == original_proposer.address)
            .unwrap();

        let parts = make_signed_parts(
            height,
            restream_round,
            pol_round,
            signing_key.public_key(),
            signing_key,
        )
        .await;

        // Resolve via pol_round (the fix) — should match and verify
        let expected = resolve_expected_proposer(&selector, &validator_set, &parts);
        let provider = LocalSigningProvider::new(signing_key.clone());
        assert!(validate_proposal_parts(&parts, expected, &provider).await);
    }

    /// Restreamed parts would fail validation if we used parts_round instead
    /// of pol_round to resolve the expected proposer (the old buggy behavior).
    #[tokio::test]
    async fn restreamed_parts_fail_validation_with_wrong_round_proposer() {
        let selector = RoundRobin;
        let (keys, validator_set) = make_validator_set(3);

        let height = Height::new(1);
        let pol_round = Round::new(0);
        let restream_round = Round::new(2);

        let original_proposer = selector.select_proposer(&validator_set, height, pol_round);
        let wrong_proposer = selector.select_proposer(&validator_set, height, restream_round);

        assert_ne!(original_proposer.address, wrong_proposer.address);

        let signing_key = keys
            .iter()
            .find(|k| Address::from_public_key(&k.public_key()) == original_proposer.address)
            .unwrap();

        let parts = make_signed_parts(
            height,
            restream_round,
            pol_round,
            signing_key.public_key(),
            signing_key,
        )
        .await;

        // Using parts_round (the old bug) resolves to the wrong proposer → validation fails
        let provider = LocalSigningProvider::new(signing_key.clone());
        assert!(!validate_proposal_parts(&parts, wrong_proposer, &provider).await);
    }

    /// assemble_block_from_parts must preserve pol_round as valid_round.
    #[tokio::test]
    async fn assemble_block_preserves_valid_round_from_pol_round() {
        use alloy_rpc_types_engine::ExecutionPayloadV3;
        use arbitrary::{Arbitrary, Unstructured};

        let mut u = Unstructured::new(&[0u8; 512]);
        let payload = ExecutionPayloadV3::arbitrary(&mut u).unwrap();

        let (keys, _) = make_validator_set(1);
        let signing_key = &keys[0];
        let proposer = Address::from_public_key(&signing_key.public_key());

        let pol_round = Round::new(1);

        let block = ConsensusBlock {
            height: Height::new(10),
            round: Round::new(3),
            valid_round: pol_round,
            proposer,
            validity: Validity::Valid,
            execution_payload: payload,
            signature: None,
        payment_payload: None,
        };

        let provider = LocalSigningProvider::new(signing_key.clone());
        let (raw_parts, _sig) = make_proposal_parts(&provider, &block).await.unwrap();
        let parts = ProposalParts::new(raw_parts).unwrap();

        // Sanity: Init carries the pol_round we set
        assert_eq!(parts.init().pol_round, pol_round);

        let assembled = assemble_block_from_parts(&parts, None).await.unwrap();
        assert_eq!(
            assembled.valid_round, pol_round,
            "assemble_block_from_parts must propagate pol_round as valid_round"
        );
    }

    #[tokio::test]
    async fn assemble_block_preserves_nil_valid_round() {
        use alloy_rpc_types_engine::ExecutionPayloadV3;
        use arbitrary::{Arbitrary, Unstructured};

        let mut u = Unstructured::new(&[0u8; 512]);
        let payload = ExecutionPayloadV3::arbitrary(&mut u).unwrap();

        let (keys, _) = make_validator_set(1);
        let signing_key = &keys[0];
        let proposer = Address::from_public_key(&signing_key.public_key());

        let block = ConsensusBlock {
            height: Height::new(5),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer,
            validity: Validity::Valid,
            execution_payload: payload,
            signature: None,
        payment_payload: None,
        };

        let provider = LocalSigningProvider::new(signing_key.clone());
        let (raw_parts, _sig) = make_proposal_parts(&provider, &block).await.unwrap();
        let parts = ProposalParts::new(raw_parts).unwrap();

        assert_eq!(parts.init().pol_round, Round::Nil);

        let assembled = assemble_block_from_parts(&parts, None).await.unwrap();
        assert_eq!(assembled.valid_round, Round::Nil);
    }

    /// Dual-EL: a block carrying BOTH an EVM and a payment payload must round-trip
    /// through make_proposal_parts -> assemble_block_from_parts with both lanes intact.
    #[tokio::test]
    async fn assemble_block_round_trips_payment_payload() {
        use alloy_rpc_types_engine::ExecutionPayloadV3;
        use arbitrary::{Arbitrary, Unstructured};

        // Two DISTINCT payloads for the EVM lane and the payment lane.
        let mut u_evm = Unstructured::new(&[0x11u8; 1024]);
        let evm_payload = ExecutionPayloadV3::arbitrary(&mut u_evm).unwrap();
        let mut u_pay = Unstructured::new(&[0x22u8; 1024]);
        let pay_payload = ExecutionPayloadV3::arbitrary(&mut u_pay).unwrap();
        assert_ne!(
            evm_payload.payload_inner.payload_inner.state_root,
            pay_payload.payload_inner.payload_inner.state_root,
            "test setup: the two lanes must differ"
        );

        let (keys, _) = make_validator_set(1);
        let signing_key = &keys[0];
        let proposer = Address::from_public_key(&signing_key.public_key());

        let block = ConsensusBlock {
            height: Height::new(7),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer,
            validity: Validity::Valid,
            execution_payload: evm_payload.clone(),
            signature: None,
            payment_payload: Some(pay_payload.clone()),
        };

        let provider = LocalSigningProvider::new(signing_key.clone());
        let (raw_parts, _sig) = make_proposal_parts(&provider, &block).await.unwrap();
        let parts = ProposalParts::new(raw_parts).unwrap();

        let assembled = assemble_block_from_parts(&parts, None).await.unwrap();
        assert_eq!(
            assembled.execution_payload, evm_payload,
            "EVM lane must survive streaming"
        );
        assert_eq!(
            assembled.payment_payload,
            Some(pay_payload),
            "payment lane must survive streaming (both roots carried)"
        );
    }
}
