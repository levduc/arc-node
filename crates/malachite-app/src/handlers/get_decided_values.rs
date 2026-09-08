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

use std::ops::RangeInclusive;

use alloy_rpc_types_engine::ExecutionPayloadV3;
use alloy_rpc_types_eth::BlockNumberOrTag;
use arc_eth_engine::engine::Engine;
use bytesize::ByteSize;
use eyre::{eyre, WrapErr};
use tracing::{debug, error, info, warn};

use malachitebft_app_channel::app::types::codec::HasEncodedLen;
use malachitebft_app_channel::app::types::sync::RawDecidedValue;
use malachitebft_app_channel::Reply;
use malachitebft_core_types::utils::height::{DisplayRange, HeightRangeExt};
use malachitebft_core_types::{ExtendedCommitCertificate, Height as _, VoteExtensions};

use arc_consensus_types::codec::proto::ProtobufCodec;
use arc_consensus_types::sync::{Response, ValueResponse};
use arc_consensus_types::{ArcContext, Height};

use crate::block::{commit_lanes, encode_value};
use crate::metrics::AppMetrics;
use crate::state::State;
use crate::store::Store;

pub async fn handle(
    state: &mut State,
    engine: &Engine,
    lean_shim: Option<&arc_eth_engine::lean_shim::LeanShim>,
    range: RangeInclusive<Height>,
    reply: Reply<Vec<RawDecidedValue<ArcContext>>>,
) -> Result<(), eyre::Error> {
    let config = state.config().value_sync;

    if !config.enabled {
        warn!("GetDecidedValues: Sync is disabled in the configuration");
        let _ = reply.send(Vec::new());
        return Ok(());
    }

    let latest_height = state
        .store()
        .max_height()
        .await
        .wrap_err("GetDecidedValues: Failed to fetch the latest height from the state")?
        .unwrap_or_default();

    let earliest_height = state
        .store()
        .min_height()
        .await
        .wrap_err("GetDecidedValues: Failed to fetch the earliest height from the state")?
        .unwrap_or_default();

    let store = state.store().clone();
    let metrics = state.metrics().clone();
    let engine = engine.clone();
    let lean_shim = lean_shim.cloned();

    // Spawn retrieval of decided values in a separate task to avoid blocking the main application loop.
    tokio::spawn(async move {
        let values = get_decided_values(
            range,
            earliest_height..=latest_height,
            config.batch_size,
            config.max_response_size,
            store,
            engine,
            lean_shim,
            metrics,
        )
        .await
        .inspect_err(|e| {
            error!("🔴 GetDecidedValues: Error while getting decided values: {e:?}");
        })
        .unwrap_or_default();

        if let Err(e) = reply.send(values) {
            error!("🔴 GetDecidedValues: Failed to send reply: {e:?}");
        }
    });

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn get_decided_values(
    requested_range: RangeInclusive<Height>,
    available_range: RangeInclusive<Height>,
    batch_size: usize,
    max_response_size: ByteSize,
    store: Store,
    engine: Engine,
    lean_shim: Option<arc_eth_engine::lean_shim::LeanShim>,
    metrics: AppMetrics,
) -> Result<Vec<RawDecidedValue<ArcContext>>, eyre::Error> {
    let _guard = metrics.start_msg_process_timer("GetDecidedValues");

    let earliest_height = *available_range.start();
    let latest_height = *available_range.end();

    let Some(range) =
        get_clamped_request_range(requested_range, earliest_height, latest_height, batch_size)
    else {
        return Ok(Vec::new()); // Warn logged inside get_clamped_request_range
    };

    // Batch fetch all execution payloads in one RPC call
    let heights = range.clone().iter_heights().collect::<Vec<_>>();
    let block_numbers = heights
        .iter()
        .map(|height| BlockNumberOrTag::Number(height.as_u64()))
        .collect::<Vec<_>>();

    let execution_payloads = engine.eth.get_execution_payloads(&block_numbers).await?;

    // LEAN lane: fetch canonical lean block bytes by number for the same
    // heights (lockstep: lean number == EVM block number, enforced at
    // validation), so the synced value carries both lanes.
    // Exactly one lean block anchors per decided height since activation, so
    // lean_number(h) = lean_head - (latest_decided - h); pre-activation heights
    // map to <= 0 and serve as EVM-only frames (their certificates bound no
    // lean lane).
    let mut lean_bytes_by_height: Vec<Option<Vec<u8>>> = Vec::new();
    if let Some(shim) = &lean_shim {
        let head = shim.get_head().await?;
        let latest = latest_height.as_u64();
        for (h, ep) in heights.iter().zip(execution_payloads.iter()) {
            let Some(ep) = ep else {
                lean_bytes_by_height.push(None);
                continue;
            };
            let evm_hash = ep.payload_inner.payload_inner.block_hash;
            let Some(cert_vid) = store
                .get_certificate(Some(*h))
                .await?
                .map(|s| s.certificate.value_id.block_hash())
            else {
                lean_bytes_by_height.push(None);
                continue;
            };
            // Heights whose certificate bound no lean lane (pre-activation, or
            // decided EVM-only) serve EVM-only frames.
            if commit_lanes(evm_hash, None) == cert_vid {
                lean_bytes_by_height.push(None);
                continue;
            }
            // The offset guess (lean_number = head - (latest - h)) is only
            // valid when our lane is fully caught up AND every height since
            // activation carried a lean block. Neither held during the
            // 2026-08-22 freeze window — a mid-recovery head made this serve
            // WRONG blocks, every height then failed the certificate check
            // below, and the syncing peer starved on empty responses. So:
            // verify the guess against the certificate, and scan outward for
            // the block that actually reproduces it.
            let behind = latest.saturating_sub(h.as_u64());
            let guess = head.number.saturating_sub(behind).max(1);
            let mut candidates = vec![guess];
            for d in 1..=128u64 {
                if guess > d {
                    candidates.push(guess - d);
                }
                if guess + d <= head.number {
                    candidates.push(guess + d);
                }
            }
            let mut found = None;
            for n in candidates {
                if n == 0 || n > head.number {
                    continue;
                }
                if let Some(bytes) = shim.get_block_bytes(n).await? {
                    if let Ok(lane) =
                        arc_consensus_types::block::LeanLanePayload::new(bytes.clone())
                    {
                        if commit_lanes(evm_hash, Some(lane.commitment())) == cert_vid {
                            found = Some((n, bytes));
                            break;
                        }
                    }
                }
            }
            match found {
                Some((n, bytes)) => {
                    if n != guess {
                        info!(
                            height = h.as_u64(), guess, resolved = n,
                            "GetDecidedValues: lean mapping corrected by certificate scan"
                        );
                    }
                    lean_bytes_by_height.push(Some(bytes));
                }
                None => {
                    warn!(
                        height = h.as_u64(), latest, lean_head = head.number, guess,
                        "GetDecidedValues: no lean block reproduces the certificate \
                         (lane behind or gap > 128) — height not served"
                    );
                    lean_bytes_by_height.push(None);
                }
            }
        }
    } else {
        lean_bytes_by_height = vec![None; heights.len()];
    }

    let mut values = Vec::with_capacity(range.len());
    let mut total_bytes = ByteSize::b(0);

    for ((height, execution_payload), lean_bytes) in heights
        .into_iter()
        .zip(execution_payloads.into_iter())
        .zip(lean_bytes_by_height.into_iter())
    {
        let Some(execution_payload) = execution_payload else {
            debug!(%height, "No execution payload found at this height from EL, skipping");
            continue;
        };

        let (raw_value, raw_bytes_len) =
            match get_raw_decided_value(
                &store,
                execution_payload,
                lean_bytes,
                height,
                lean_shim.is_some(),
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    warn!(%height, "Failed to get decided value at height: {e}");
                    continue;
                }
            };

        // NOTE: This size estimate slightly over-approximates the true wire size.
        // These estimates assume each value is sent in its own SyncResponse message,
        // whereas in practice all values are batched into a single message.
        //
        // For 10 SyncedValues each with 10 signatures, batching all values into
        // one SyncResponse (~10X + 9,990 bytes) is about 90 bytes smaller than sending
        // 10 separate SyncResponses (~10X + 10,080 bytes). In other words, splitting
        // adds ~9 bytes of framing overhead per message (<1% overhead for typical payloads).
        //
        // This over-approximation is acceptable for our purpose of ensuring we are not going
        // over the max response size limit.
        //
        // Moreover, Malachite will perform a very similar over-approximation when checking
        // the response to GetDecidedValues, so this keeps our behavior consistent.
        #[allow(clippy::arithmetic_side_effects)]
        // Equivalent to `total_bytes + raw_bytes_len > max_response_size`,
        // but rearranged so the subtraction cannot overflow (raw_bytes_len <= max_response_size
        // is checked first, and max_response_size.0 - raw_bytes_len.0 is then non-negative).
        if raw_bytes_len > max_response_size
            || total_bytes.as_u64() > max_response_size.as_u64() - raw_bytes_len.as_u64()
        {
            warn!(
                %height, %max_response_size, %raw_bytes_len,
                "GetDecidedValues: Reached max total bytes limit for response, stopping here",
            );

            break;
        }

        #[allow(clippy::arithmetic_side_effects)] // Guarded by the comparison above
        {
            total_bytes += raw_bytes_len;
        }

        values.push(raw_value);
    }

    info!(
        values = %values.len(),
        %total_bytes,
        %max_response_size,
        "GetDecidedValues: Returning decided values"
    );

    Ok(values)
}

async fn get_raw_decided_value(
    store: &Store,
    execution_payload: ExecutionPayloadV3,
    lean_bytes: Option<Vec<u8>>,
    height: Height,
    lean_lane_enabled: bool,
) -> eyre::Result<(RawDecidedValue<ArcContext>, ByteSize)> {
    let stored = store
        .get_certificate(Some(height))
        .await?
        .ok_or_else(|| eyre!("No certificate found at height {height}"))?;

    // Verify the lanes we fetched reproduce the committed value_id before shipping
    // them to a peer. Guards against a lean-node mismatch (wrong/missing block)
    // that would otherwise send a value the peer cannot validate against the cert.
    // With the lane off this is exactly the EVM block hash check `DecidedBlock::new`
    // used to assert.
    let evm_block_hash = execution_payload.payload_inner.payload_inner.block_hash;
    let lean_lane = lean_bytes
        .map(arc_consensus_types::block::LeanLanePayload::new)
        .transpose()
        .wrap_err_with(|| {
            format!("lean lane: fetched block bytes failed strict decode at height {height}")
        })?;
    let lane_commitment = lean_lane.as_ref().map(|l| l.commitment());
    let value_id = commit_lanes(evm_block_hash, lane_commitment);
    if value_id != stored.certificate.value_id.block_hash() {
        return Err(eyre!(
            "commitment over fetched lanes ({value_id}) does not match certificate value_id ({}) at height {height}",
            stored.certificate.value_id,
        ));
    }

    let value_bytes = encode_value(
        &execution_payload,
        lean_lane.as_ref().map(|l| l.bytes.as_slice()),
        lean_lane_enabled,
    );
    debug!(
        height = height.as_u64(),
        len = value_bytes.len(),
        lean = lean_lane.is_some(),
        "GetDecidedValues: serving frame"
    );
    let certificate = ExtendedCommitCertificate::from_commit_certificate_and_extensions(
        stored.certificate,
        VoteExtensions::default(),
    );

    let raw_value = RawDecidedValue {
        certificate,
        value_bytes: value_bytes.into(),
    };

    let response = Response::ValueResponse(ValueResponse::new(height, vec![raw_value.clone()]));

    let Ok(raw_bytes_len) = ProtobufCodec.encoded_len(&response) else {
        return Err(eyre!(
            "Failed to determine encoded length of value at height {height}"
        ));
    };

    // encoded_len returns usize; on 64-bit targets this fits in u64
    #[allow(clippy::cast_possible_truncation)]
    Ok((raw_value, ByteSize::b(raw_bytes_len as u64)))
}

fn get_clamped_request_range(
    range: RangeInclusive<Height>,
    earliest_height: Height,
    latest_height: Height,
    batch_size: usize,
) -> Option<RangeInclusive<Height>> {
    assert!(
        earliest_height <= latest_height,
        "Earliest height must always be less than or equal to latest height"
    );
    let mut start = *range.start();
    let mut end = *range.end();

    if end < start {
        warn!(requested_start = %start, requested_end = %end, "GetDecidedValues: Invalid inverted request range");
        return None;
    }

    if end < earliest_height || start > latest_height {
        warn!(
            requested_start = %start,
            requested_end = %end,
            %earliest_height,
            %latest_height,
            "GetDecidedValues: Requested range lies wholly outside available bounds",
        );
        return None;
    }

    if start < earliest_height {
        warn!(
            %earliest_height,
            requested_start = %start,
            "GetDecidedValues: Requested start is before earliest height; clamping",
        );
        start = earliest_height;
    }
    if end > latest_height {
        warn!(
            %latest_height,
            requested_end = %end,
            "GetDecidedValues: Requested end is beyond latest height; clamping",
        );
        end = latest_height;
    }

    debug_assert!(start <= end, "Post-clamp range must satisfy start <= end");

    if batch_size == 0 {
        warn!(
            requested = %DisplayRange(&(start..=end)),
            "GetDecidedValues: Batch size is zero; returning None",
        );
        return None;
    }

    // start <= end guaranteed by clamp logic above; +1 cannot overflow after clamping
    // to real block heights, but we handle it gracefully regardless.
    #[allow(clippy::arithmetic_side_effects)]
    let Some(requested_count) = (end.as_u64() - start.as_u64()).checked_add(1) else {
        warn!("GetDecidedValues: height range count overflow");
        return None;
    };
    // batch_size > 0 checked above; fits in u64 on 64-bit targets
    #[allow(clippy::cast_possible_truncation)]
    if requested_count > batch_size as u64 {
        end = start.increment_by(batch_size.saturating_sub(1) as u64);
        warn!(
            requested = %requested_count,
            max = %batch_size,
            clamped = %DisplayRange(&(start..=end)),
            "GetDecidedValues: Clamping request range to max batch size",
        );
    }

    Some(start..=end)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to easily create ranges
    fn h(start: u64, end: u64) -> RangeInclusive<Height> {
        Height::new(start)..=Height::new(end)
    }

    #[test]
    fn test_returns_range_unchanged_when_within_limits() {
        let range = h(10, 14);
        let batch_size = 10;
        let earliest = Height::new(5);
        let latest = Height::new(20);

        let result =
            get_clamped_request_range(range.clone(), earliest, latest, batch_size).unwrap();
        assert_eq!(result, range);
    }

    #[test]
    fn test_clamps_when_range_exceeds_batch_size() {
        let range = h(10, 25); // 16 items total
        let batch_size = 10;
        let earliest = Height::new(5);
        let latest = Height::new(50);

        let result = get_clamped_request_range(range, earliest, latest, batch_size).unwrap();
        let expected = h(10, 19); // 10 total values: 10..=19

        assert_eq!(result, expected);
    }

    #[test]
    fn test_clamps_when_end_exceeds_latest_height() {
        let range = h(10, 25);
        let batch_size = 20;
        let earliest = Height::new(5);
        let latest = Height::new(15);

        let result = get_clamped_request_range(range, earliest, latest, batch_size).unwrap();
        let expected = h(10, 15);

        assert_eq!(result, expected);
    }

    #[test]
    fn test_clamps_for_both_batch_and_latest_height() {
        let range = h(10, 50);
        let batch_size = 10;
        let earliest = Height::new(5);
        let latest = Height::new(15);

        let result = get_clamped_request_range(range, earliest, latest, batch_size).unwrap();
        // First clamp: 10..=19 (batch)
        // Then clamp again: 10..=15 (latest)
        let expected = h(10, 15);

        assert_eq!(result, expected);
    }

    #[test]
    fn test_handles_range_equal_to_latest_height() {
        let range = h(10, 20);
        let batch_size = 15;
        let earliest = Height::new(5);
        let latest = Height::new(20);

        let result =
            get_clamped_request_range(range.clone(), earliest, latest, batch_size).unwrap();
        assert_eq!(result, range);
    }

    #[test]
    fn test_handles_singleton_range() {
        // When start == end, still valid range
        let range = h(5, 5);
        let batch_size = 10;
        let earliest = Height::new(1);
        let latest = Height::new(100);

        let result =
            get_clamped_request_range(range.clone(), earliest, latest, batch_size).unwrap();
        assert_eq!(result, range);
    }

    #[test]
    fn test_handles_zero_batch_size_is_none() {
        let range = h(10, 20);
        let batch_size = 0;
        let earliest = Height::new(5);
        let latest = Height::new(50);

        let res = get_clamped_request_range(range, earliest, latest, batch_size);
        assert!(res.is_none());
    }

    #[test]
    fn test_clamps_request_range_within_bounds() {
        let range = h(1, 10);
        let earliest = Height::new(1);
        let latest = Height::new(20);
        let batch_size = 100;
        let result =
            get_clamped_request_range(range.clone(), earliest, latest, batch_size).unwrap();
        assert_eq!(result, range);
    }

    #[test]
    fn test_clamp_request_range_end_beyond_tip() {
        let range = h(5, 15);
        let earliest = Height::new(1);
        let latest = Height::new(10);
        let batch_size = 100;
        let result =
            get_clamped_request_range(range.clone(), earliest, latest, batch_size).unwrap();
        assert_eq!(result, h(5, 10));
    }

    #[test]
    fn test_clamp_request_range_start_and_end_at_tip() {
        let range = h(10, 10);
        let earliest = Height::new(1);
        let latest = Height::new(10);
        let batch_size = 100;
        let result =
            get_clamped_request_range(range.clone(), earliest, latest, batch_size).unwrap();
        assert_eq!(result, range);
    }

    #[test]
    fn test_clamp_request_range_start_above_tip() {
        let range = h(15, 20);
        let earliest = Height::new(1);
        let latest = Height::new(10);
        let batch_size = 10;
        let res = get_clamped_request_range(range.clone(), earliest, latest, batch_size);
        assert!(res.is_none());
    }

    #[test]
    fn test_inverted_above_bounds_is_none() {
        let range = h(20, 10);
        let earliest = Height::new(3);
        let latest = Height::new(8);
        let batch_size = 20;
        let res = get_clamped_request_range(range, earliest, latest, batch_size);
        assert!(res.is_none());
    }

    #[test]
    fn test_inverted_below_bounds_is_none() {
        let range = h(20, 19);
        let earliest = Height::new(40);
        let latest = Height::new(80);
        let batch_size = 10;
        let res = get_clamped_request_range(range, earliest, latest, batch_size);
        assert!(res.is_none());
    }

    #[test]
    fn test_inverted_within_bounds_is_none() {
        let range = h(20, 19);
        let earliest = Height::new(5);
        let latest = Height::new(50);
        let batch_size = 10;
        let res = get_clamped_request_range(range, earliest, latest, batch_size);
        assert!(res.is_none());
    }

    #[test]
    fn test_entirely_below_bounds_is_none() {
        let range = h(1, 2);
        let earliest = Height::new(5);
        let latest = Height::new(50);
        let batch_size = 10;
        let res = get_clamped_request_range(range, earliest, latest, batch_size);
        assert!(res.is_none());
    }

    #[test]
    fn test_entirely_above_bounds_is_none() {
        let range = h(60, 65);
        let earliest = Height::new(5);
        let latest = Height::new(50);
        let batch_size = 10;
        let res = get_clamped_request_range(range, earliest, latest, batch_size);
        assert!(res.is_none());
    }

    #[test]
    fn test_batch_cap_near_latest() {
        let range = h(95, 120);
        let earliest = Height::new(50);
        let latest = Height::new(100);
        let batch_size = 10;
        let result = get_clamped_request_range(range, earliest, latest, batch_size).unwrap();
        assert_eq!(result, h(95, 100));
    }

    #[test]
    fn test_bounds_first_then_batch() {
        let range = h(1, 100);
        let earliest = Height::new(20);
        let latest = Height::new(200);
        let batch_size = 5;
        let result = get_clamped_request_range(range, earliest, latest, batch_size).unwrap();
        assert_eq!(result, h(20, 24));
    }

    #[test]
    fn test_exact_batch_size_no_change() {
        let range = h(10, 19);
        let earliest = Height::new(1);
        let latest = Height::new(100);
        let batch_size = 10;
        let result =
            get_clamped_request_range(range.clone(), earliest, latest, batch_size).unwrap();
        assert_eq!(result, range);
    }

    #[test]
    fn test_exact_limits() {
        let range = h(20, 29);
        let earliest = Height::new(20);
        let latest = Height::new(29);
        let batch_size = 10;
        let result =
            get_clamped_request_range(range.clone(), earliest, latest, batch_size).unwrap();
        assert_eq!(result, range);
    }

    #[test]
    fn test_off_by_1() {
        let range = h(19, 29);
        let earliest = Height::new(20);
        let latest = Height::new(28);
        let batch_size = 8;
        let result =
            get_clamped_request_range(range.clone(), earliest, latest, batch_size).unwrap();
        assert_eq!(result, h(20, 27));
    }
}
