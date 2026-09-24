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

//! Golden-file tests for the rendered compose files.
//!
//! The snapshots pin the exact bytes quake writes for a fixed manifest, so a
//! template change that is meant to be inert for existing scenarios (for
//! example an optional service behind a flag) is proven inert.
//!
//! Regenerate after an intended change with
//! `QUAKE_UPDATE_SNAPSHOTS=1 cargo test -p quake compose_snapshot`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use indexmap::IndexMap;

use super::*;
use crate::infra::{InfraData, InfraType};
use crate::manifest::Manifest;
use crate::nodes::NodesMetadata;

const LOCAL_TEMPLATE: &str = include_str!("../../templates/local/compose.yaml.hbs");
const REMOTE_TEMPLATE: &str = include_str!("../../templates/remote/compose-node.yaml.hbs");

/// Three validators with a global and a per-node env var: enough to exercise
/// every per-node section of both templates.
pub(super) const BASE_MANIFEST: &str = r#"
[cl.env]
CL_GLOBAL = "1"

[nodes.validator1]
cl.env = { CL_NODE = "one" }
[nodes.validator2]
[nodes.validator3]
"#;

pub(super) fn test_images() -> testnet::DockerImages {
    testnet::DockerImages {
        cl: "ghcr.io/test/arc-consensus:snap".to_string(),
        el: "ghcr.io/test/arc-execution:snap".to_string(),
        cl_upgrade: None,
        el_upgrade: None,
        lean: None,
    }
}

/// Node metadata for `manifest`, local or remote (remote nodes get fixed VPC IPs).
pub(super) fn metadata(
    manifest: &Manifest,
    images: &testnet::DockerImages,
    infra: InfraType,
) -> NodesMetadata {
    let local = InfraData::new_local("testnet".to_string(), &manifest.nodes);
    let infra_data = match infra {
        InfraType::Local => local,
        InfraType::Remote => {
            let mut v = serde_json::to_value(&local).unwrap();
            v["provider"] = serde_json::to_value(InfraType::Remote).unwrap();
            for (i, name) in manifest.nodes.keys().enumerate() {
                v["nodes"][name]["subnet_ips"] =
                    serde_json::json!({ "default": format!("10.0.1.{}", 10 + i) });
            }
            serde_json::from_value(v).unwrap()
        }
    };
    let mut md = NodesMetadata::new(infra_data, manifest, images, &BTreeSet::new()).unwrap();
    for (name, node) in md.nodes.iter_mut() {
        node.consensus
            .set_cli_flags(vec![format!("--moniker={name}")]);
    }
    md
}

pub(super) fn render_local(manifest: &Manifest, images: &testnet::DockerImages) -> String {
    let md = metadata(manifest, images, InfraType::Local);
    let data = ComposeTemplateDataLocal {
        compose_project_name: "quake".to_string(),
        nodes: md.values(),
        networks: build_template_networks(&manifest.subnets.cidr_map()),
        deployments_dir: "../../deployments".to_string(),
        quake_dir: "..".to_string(),
        images: images.clone(),
        rpc: false,
        reth_builds: vec![],
        malachite_builds: vec![],
        latency_emulation: true,
        monitoring_bind_host: None,
        trusted_peers: md
            .node_names()
            .into_iter()
            .map(|n| (n, None))
            .collect::<IndexMap<_, _>>(),
        el_cpu_limit: None,
        el_memory_limit_gb: None,
        cl_cpu_limit: None,
        cl_memory_limit_gb: None,
    };
    render_compose(LOCAL_TEMPLATE, &data).unwrap()
}

pub(super) fn render_remote(
    manifest: &Manifest,
    images: &testnet::DockerImages,
    node: &str,
) -> String {
    let md = metadata(manifest, images, InfraType::Remote);
    let meta = md.get(&node.to_string()).unwrap();
    let data = ComposeTemplateDataRemote {
        compose_project_name: "quake".to_string(),
        cl_container_name: "cl".to_string(),
        el_container_name: "el".to_string(),
        node_name: node.to_string(),
        latency_emulation: true,
        rpc: false,
        remote_home_dir: "/home/ssm-user".to_string(),
        images: images.clone(),
        cl_cli_flags: meta.consensus.cli_flags.clone(),
        el_cli_flags: vec![],
        trusted_peers: None,
        el_cpu_limit: None,
        el_memory_limit_gb: None,
        cl_cpu_limit: None,
        cl_memory_limit_gb: None,
        el_env: meta.el_env.clone(),
        cl_env: meta.cl_env.clone(),
    };
    render_compose(REMOTE_TEMPLATE, &data).unwrap()
}

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/setup/snapshots")
        .join(name)
}

/// Compare `rendered` with the checked-in snapshot `name`, or rewrite it when
/// `QUAKE_UPDATE_SNAPSHOTS` is set.
fn assert_snapshot(name: &str, rendered: &str) {
    let path = snapshot_path(name);
    if std::env::var_os("QUAKE_UPDATE_SNAPSHOTS").is_some() {
        fs::write(&path, rendered).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing snapshot {}: {e}", path.display()));
    if expected != rendered {
        let first_diff = expected
            .lines()
            .zip(rendered.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(expected.lines().count().min(rendered.lines().count()));
        panic!(
            "rendered compose differs from snapshot {} (first differing line {}):\n\
             expected: {:?}\n  actual: {:?}",
            path.display(),
            first_diff + 1,
            expected.lines().nth(first_diff),
            rendered.lines().nth(first_diff),
        );
    }
}

#[test]
fn compose_snapshot_local() {
    let manifest = Manifest::from_string(BASE_MANIFEST).unwrap();
    assert_snapshot(
        "compose-local.yaml",
        &render_local(&manifest, &test_images()),
    );
}

#[test]
fn compose_snapshot_remote() {
    let manifest = Manifest::from_string(BASE_MANIFEST).unwrap();
    assert_snapshot(
        "compose-remote-validator1.yaml",
        &render_remote(&manifest, &test_images(), "validator1"),
    );
}
