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

//! The lean payment lane as a quake service.
//!
//! When a manifest carries `[lean] enabled = true`, every node gets a third
//! container (`<node>_lean`) running the lean lane node, and quake owns the
//! consensus layer's lane wiring: it injects the `ARC_PAYMENT_LEAN_*`
//! environment into each CL and generates the lane's genesis fund file. With
//! the section absent (or disabled) nothing here is used and the rendered
//! testnet files are identical to a lane-less quake.

use std::fs;
use std::path::Path;

use alloy_signer_local::{coins_bip39::English, MnemonicBuilder};
use color_eyre::eyre::{bail, Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Port the lean node serves JSON-RPC and WebSocket on, inside its container.
pub(crate) const LEAN_RPC_PORT: usize = 8560;
/// Lane chain id (signing domain and genesis commitment of the lean chain).
pub(crate) const LEAN_CHAIN_ID: u64 = 1338;
/// Genesis fund file, in the testnet's shared `assets/` directory.
pub(crate) const FUND_FILE_NAME: &str = "lean-fund.txt";
/// Where the lean container sees the fund file.
pub(crate) const FUND_FILE_CONTAINER_PATH: &str = "/fund/lean-fund.txt";
/// Data directory (block log, snapshots) inside the lean container.
pub(crate) const DATA_DIR_CONTAINER_PATH: &str = "/data";

/// Mnemonic the load generators sign with (the standard test mnemonic).
const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";
/// Derivation path of the first funded account. The lean spammer signs with
/// `m/44'/60'/1'/0/i` (note the `1'`), so the fund file must use the same path.
const FUND_DERIVATION_PATH: &str = "m/44'/60'/1'/0/0";

/// Environment variables quake sets on every CL when the lane is enabled.
/// A manifest must not set them itself.
pub(crate) const CL_ENV_PREFIX: &str = "ARC_PAYMENT_LEAN_";

/// `[lean]` manifest section.
///
/// ```toml
/// [lean]
/// enabled = true
/// budget_gas = 100_000_000          # per-block lean gas budget
/// fund_accounts = 200               # funded accounts at m/44'/60'/1'/0/i
/// fund_balance = "10000000000000000000"  # wei per account (a string: > i64)
/// fanout_outputs = 100              # default fan-out N for lean load
/// image = "ghcr.io/org/lean-lane:tag"    # optional image override
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LeanConfig {
    /// Run the lean lane. `false` is the same as omitting the section.
    #[serde(default)]
    pub enabled: bool,
    /// Per-block lean gas budget (`ARC_PAYMENT_LEAN_BUDGET_GAS` on every CL).
    #[serde(default = "default_budget_gas")]
    pub budget_gas: u64,
    /// Number of accounts in the genesis fund file.
    #[serde(default = "default_fund_accounts")]
    pub fund_accounts: usize,
    /// Genesis balance of each funded account, in wei, as a decimal string
    /// (TOML integers stop at i64, below the balances the lane needs).
    #[serde(default = "default_fund_balance")]
    pub fund_balance: String,
    /// Default number of payments per fan-out transaction for lean load.
    #[serde(default = "default_fanout_outputs")]
    pub fanout_outputs: u32,
    /// Lean node image; defaults to `lean-lane:local` locally and
    /// `${IMAGE_REGISTRY_URL}/lean-lane:latest` remotely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

fn default_budget_gas() -> u64 {
    100_000_000
}
fn default_fund_accounts() -> usize {
    200
}
fn default_fund_balance() -> String {
    "10000000000000000000".to_string()
}
fn default_fanout_outputs() -> u32 {
    10
}

impl LeanConfig {
    /// Check the values that the manifest cannot type-check on its own.
    pub fn validate(&self) -> Result<()> {
        if self.budget_gas == 0 {
            bail!("[lean] budget_gas must be greater than 0");
        }
        if self.fund_accounts == 0 {
            bail!("[lean] fund_accounts must be greater than 0");
        }
        match self.fund_balance.parse::<u128>() {
            Ok(0) | Err(_) => bail!(
                "[lean] fund_balance must be a positive integer number of wei (got '{}')",
                self.fund_balance
            ),
            Ok(_) => {}
        }
        if self.fanout_outputs == 0 {
            bail!("[lean] fanout_outputs must be greater than 0");
        }
        Ok(())
    }

    /// The `ARC_PAYMENT_LEAN_*` environment of one CL: lane on, the budget,
    /// its own lean node and the peer lean nodes it may catch up from.
    pub fn cl_env(&self, own_rpc: &str, peer_rpcs: &[String]) -> IndexMap<String, String> {
        IndexMap::from([
            ("ARC_PAYMENT_LEAN_LANE".to_string(), "1".to_string()),
            (
                "ARC_PAYMENT_LEAN_BUDGET_GAS".to_string(),
                self.budget_gas.to_string(),
            ),
            ("ARC_PAYMENT_LEAN_RPC".to_string(), own_rpc.to_string()),
            (
                "ARC_PAYMENT_LEAN_PEER_RPCS".to_string(),
                peer_rpcs.join(","),
            ),
        ])
    }

    /// Arguments of one lean node container (`lean-lane-node <args>`), given
    /// the RPC URLs of its peer lean nodes.
    pub fn node_args(&self, peer_rpcs: &[String]) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            format!("--datadir={DATA_DIR_CONTAINER_PATH}"),
            // The node binds loopback by default; peers and the CL are remote.
            "--bind=0.0.0.0".to_string(),
            format!("--port={LEAN_RPC_PORT}"),
            format!("--chain-id={LEAN_CHAIN_ID}"),
            format!("--fund-file={FUND_FILE_CONTAINER_PATH}"),
            format!("--fund-balance={}", self.fund_balance),
        ];
        if !peer_rpcs.is_empty() {
            args.push(format!("--peers={}", peer_rpcs.join(",")));
        }
        args
    }
}

/// The first `count` addresses at `m/44'/60'/1'/0/i` of the test mnemonic,
/// EIP-55 checksummed: the accounts the lean spammer signs with.
pub(crate) fn fund_addresses(count: usize) -> Result<Vec<String>> {
    let builder = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .derivation_path(FUND_DERIVATION_PATH)
        .wrap_err("invalid fund derivation path")?;
    builder
        .into_iter()
        .take(count)
        .map(|signer| {
            Ok(signer
                .wrap_err("failed to derive fund account")?
                .address()
                .to_string())
        })
        .collect()
}

/// Write the lane's genesis fund file (one address per line) into `assets_dir`.
///
/// Every lean node must start from the same file (it is part of the lean
/// genesis commitment), so it is written once per testnet into the shared
/// assets. An existing file with the right number of accounts is kept unless
/// `force` is set.
pub(crate) fn generate_fund_file(assets_dir: &Path, count: usize, force: bool) -> Result<()> {
    let path = assets_dir.join(FUND_FILE_NAME);
    if !force {
        if let Ok(existing) = fs::read_to_string(&path) {
            if existing.lines().count() == count {
                debug!(path=%path.display(), "⏭️ Skipping generating lean fund file");
                return Ok(());
            }
        }
    }
    let mut content = fund_addresses(count)?.join("\n");
    content.push('\n');
    fs::write(&path, content)
        .with_context(|| format!("Failed to write lean fund file {}", path.display()))?;
    debug!(path=%path.display(), count, "✅ Generated lean fund file");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cast wallet address --mnemonic "test ... junk" --mnemonic-derivation-path
    /// "m/44'/60'/1'/0/i"` for i = 0, 1, 2 — what `gen-lean-fund.sh` writes. The
    /// first also heads the fleet's `lean-fund.txt`.
    const FIRST_FUND_ADDRESS: &str = "0x8C8d35429F74ec245F8Ef2f4Fd1e551cFF97d650";
    const CAST_FUND_ADDRESSES: [&str; 3] = [
        FIRST_FUND_ADDRESS,
        "0x40FBBE484b8Ee6139Af08446950B088e10b2306A",
        "0x2b382887D362cCae885a421C978c7e998D3c95a6",
    ];

    #[test]
    fn fund_addresses_match_the_spammer_derivation() {
        assert_eq!(fund_addresses(3).unwrap(), CAST_FUND_ADDRESSES);
    }

    #[test]
    fn fund_file_is_deterministic_and_sized() {
        let dir = tempfile::tempdir().unwrap();
        generate_fund_file(dir.path(), 4, false).unwrap();
        let first = fs::read_to_string(dir.path().join(FUND_FILE_NAME)).unwrap();
        assert_eq!(first.lines().count(), 4);
        assert_eq!(first.lines().next().unwrap(), FIRST_FUND_ADDRESS);

        // Regenerating yields the same bytes.
        generate_fund_file(dir.path(), 4, true).unwrap();
        let again = fs::read_to_string(dir.path().join(FUND_FILE_NAME)).unwrap();
        assert_eq!(first, again);

        // A different size rewrites the file even without force.
        generate_fund_file(dir.path(), 2, false).unwrap();
        let resized = fs::read_to_string(dir.path().join(FUND_FILE_NAME)).unwrap();
        assert_eq!(resized.lines().count(), 2);
        assert!(first.starts_with(&resized));
    }

    #[test]
    fn validate_rejects_bad_values() {
        let ok = LeanConfig {
            enabled: true,
            budget_gas: 1,
            fund_accounts: 1,
            fund_balance: "1".to_string(),
            fanout_outputs: 1,
            image: None,
        };
        ok.validate().unwrap();
        for bad in [
            LeanConfig {
                budget_gas: 0,
                ..ok.clone()
            },
            LeanConfig {
                fund_accounts: 0,
                ..ok.clone()
            },
            LeanConfig {
                fund_balance: "0".to_string(),
                ..ok.clone()
            },
            LeanConfig {
                fund_balance: "1e19".to_string(),
                ..ok.clone()
            },
            LeanConfig {
                fanout_outputs: 0,
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "accepted {bad:?}");
        }
    }
}
