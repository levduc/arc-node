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

//! The LEAN payment lane's consensus-layer logic, in one place.
//!
//! Everything here is reached only when `ARC_PAYMENT_LEAN_LANE=1` put a
//! `LeanShim` in the handlers' hands; with the flag off the handlers pass
//! `None` and none of this runs. Keeping it out of the handler bodies is what
//! makes each flag-gated arm a single call, and what makes the lane portable:
//! the delta against upstream is this directory plus one call per arm.

pub(crate) mod anchor;
pub(crate) mod catchup;
#[cfg(test)]
pub(crate) mod test_lane;

use arc_eth_engine::transient::{is_transient, TransientDependencyError};
use tracing::warn;

use arc_consensus_types::Height;

use crate::metrics::app::{AppMetrics, LeanNoVerdictReason};

/// "No verdict from the lean lane this round" — an abstain, carrying WHY in a
/// form the handlers can label a counter with instead of parsing text.
///
/// It is transient by construction: either the dependency error that caused it
/// sits underneath, or a [`TransientDependencyError`] marker does, so
/// [`is_transient`] keeps answering true through every `wrap_err` the handlers
/// add on the way up.
#[derive(Debug)]
pub struct LeanNoVerdict {
    pub reason: LeanNoVerdictReason,
    detail: String,
    /// The transient marker, when the dependency that caused this was AWAY.
    /// `None` when the cause was a real (non-transient) failure, so this
    /// wrapper never launders one into "just wait for the node".
    source: Option<TransientDependencyError>,
}

impl std::fmt::Display for LeanNoVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for LeanNoVerdict {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// "No verdict this round" as a transient error, so the handlers skip /
/// re-request instead of dying (see [`is_transient`]).
pub(crate) fn lean_no_verdict(reason: LeanNoVerdictReason, detail: String) -> eyre::Report {
    eyre::Report::new(LeanNoVerdict {
        reason,
        detail,
        source: Some(TransientDependencyError::new(
            "lean lane node",
            format!("lean lane: no verdict ({reason})"),
        )),
    })
}

/// Same, but over a dependency error that already explains itself.
///
/// The cause is FLATTENED into the message rather than kept as the source: a
/// boxed `eyre::Report` is opaque to [`is_transient`]'s downcast, so keeping
/// it would silently lose the transient marker (and with it the "skip the
/// round" behaviour). The classification is re-derived from the cause here
/// instead, so a genuinely non-transient failure stays non-transient.
pub(crate) fn lean_no_verdict_over(
    reason: LeanNoVerdictReason,
    detail: impl std::fmt::Display,
    cause: eyre::Report,
) -> eyre::Report {
    let detail = format!("{detail}: {cause:#}");
    if is_transient(&cause) {
        return lean_no_verdict(reason, detail);
    }
    eyre::Report::new(LeanNoVerdict {
        reason,
        detail,
        source: None,
    })
}

/// The lean lane's reason for abstaining, if this report is one — at the root
/// or anywhere under the `wrap_err` layers the handlers add.
pub fn lean_no_verdict_reason(report: &eyre::Report) -> Option<LeanNoVerdictReason> {
    report
        .downcast_ref::<LeanNoVerdict>()
        .map(|e| e.reason)
        .or_else(|| {
            report
                .chain()
                .find_map(|e| e.downcast_ref::<LeanNoVerdict>().map(|e| e.reason))
        })
}

/// Height of the last lean abstain we logged, so a validator that abstains on
/// every round of a stuck height says so once rather than per round. `u64::MAX`
/// is the "nothing logged yet" sentinel (no real height reaches it).
static LAST_LEAN_ABSTAIN_WARN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Count (and, once per height, log) an abstain caused by the LEAN lane.
///
/// Called by every handler that turns a no-verdict into "no vote": the vote
/// path, the round-start paths and value-sync. A no-op for any other error, so
/// callers can hand it every validation failure they see. Without this, a
/// validator whose lean node is behind abstains silently — containers up,
/// agreement passing, no counter moving (CLAUDE.md §6).
pub fn note_lean_abstain(metrics: &AppMetrics, height: Height, err: &eyre::Report) {
    let Some(reason) = lean_no_verdict_reason(err) else {
        return;
    };
    metrics.inc_lean_no_verdict(reason);
    let height_u64 = height.as_u64();
    if LAST_LEAN_ABSTAIN_WARN.swap(height_u64, std::sync::atomic::Ordering::Relaxed) != height_u64 {
        warn!(
            %height, %reason,
            "🪶 lean lane: no verdict — this node is abstaining for this height: {err:#}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "No verdict" must reach the handlers as a TRANSIENT error, so they skip
    /// the round instead of dying (and never record an Invalid).
    #[test]
    fn no_verdict_is_a_transient_error() {
        let err = lean_no_verdict(
            LeanNoVerdictReason::BudgetExhausted,
            "lean lane: still behind after catch-up".to_string(),
        );
        assert!(is_transient(&err), "marker lost: {err:#}");
    }

    /// The reason must survive the `wrap_err` layers the handlers add, or the
    /// abstain gets counted as "unlabelled" exactly when it matters.
    #[test]
    fn the_abstain_reason_survives_wrapping() {
        let err = lean_no_verdict(
            LeanNoVerdictReason::PeersTimedOut,
            "lean lane: still behind after catch-up".to_string(),
        )
        .wrap_err("Payload validation failed on block built after receiving proposal part");

        assert_eq!(
            lean_no_verdict_reason(&err),
            Some(LeanNoVerdictReason::PeersTimedOut)
        );
        assert!(is_transient(&err), "marker lost: {err:#}");
    }

    /// An error that has nothing to do with the lean lane must not be counted
    /// as a lean abstain — the counter is only useful if it is specific.
    #[test]
    fn a_plain_error_is_not_a_lean_abstain() {
        let metrics = AppMetrics::default();
        let err = eyre::eyre!("engine call failed");

        note_lean_abstain(&metrics, Height::new(7), &err);

        assert_eq!(
            LeanNoVerdictReason::ALL
                .iter()
                .map(|r| metrics.get_lean_no_verdict(*r))
                .sum::<u64>(),
            0
        );
    }

    /// The counter the health gate reads: every abstain lands on it, labelled.
    #[test]
    fn a_lean_abstain_is_counted_under_its_reason() {
        let metrics = AppMetrics::default();
        let err = lean_no_verdict(
            LeanNoVerdictReason::LocalUnreachable,
            "lean lane: node unreachable during validation".to_string(),
        )
        .wrap_err("Payload validation failed");

        note_lean_abstain(&metrics, Height::new(7), &err);
        note_lean_abstain(&metrics, Height::new(7), &err);

        assert_eq!(
            metrics.get_lean_no_verdict(LeanNoVerdictReason::LocalUnreachable),
            2,
            "every abstain counts, even when only the first one logs"
        );
        assert_eq!(
            metrics.get_lean_no_verdict(LeanNoVerdictReason::PeersTimedOut),
            0
        );
    }
}
