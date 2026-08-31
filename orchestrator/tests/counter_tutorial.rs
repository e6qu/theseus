// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tutorial driver: runs the counter guest through the explorer with and
//! without a partition, and proves the partition-injected retry bug and its
//! bit-for-bit replay. Requires KVM.

#![cfg(target_arch = "aarch64")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use theseus_orchestrator::orchestrator::explorer::{Explorer, ExplorerConfig};
use vmm::builder::build_microvm_for_boot;
use vmm::resources::VmResources;
use vmm::seccomp::get_empty_filters;
use vmm::test_utils::mock_resources::MockVmResources;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::entropy::EntropyDeviceConfig;
use vmm::vmm_config::instance_info::InstanceInfo;
use vmm::{EventManager, Vmm};

fn guest_kernel() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../examples/counter-guest/counter_guest.bin");
    path.to_str().unwrap().to_string()
}

fn guest_resources() -> VmResources {
    let mut resources: VmResources = MockVmResources::new()
        .with_boot_source(BootSourceConfig {
            kernel_image_path: guest_kernel(),
            boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
            initrd_path: None,
        })
        .into();
    resources
        .entropy
        .insert(EntropyDeviceConfig {
            rate_limiter: None,
            seed: Some(42),
            script: None,
        })
        .unwrap();
    resources
}

/// Run one root timeline through the explorer with the given event schedule
/// and return the marker stream the guest emitted.
fn run_timeline(events: Vec<u8>) -> Vec<u8> {
    let seccomp_filters = get_empty_filters();
    let mut root_evmgr = EventManager::new().unwrap();
    let explorer = Explorer::explore(
        42,
        &ExplorerConfig {
            events,
            branch_event_suffix: false,
            rendezvous: true,
            faults: None,
            run_ms: 300,
            branches_per_node: 0,
            max_depth: 0,
        },
        &InstanceInfo::default(),
        &seccomp_filters,
        &mut root_evmgr,
        |instance_info, evmgr, filters| {
            Ok(build_microvm_for_boot(instance_info, &guest_resources(), evmgr, filters)?)
        },
        &VmResources::default,
    )
    .unwrap();
    explorer
        .tree
        .node(0)
        .payload
        .as_ref()
        .unwrap()
        .markers
        .clone()
}

#[test]
fn counter_tutorial() {
    // Two increments, no partition: each applied exactly once.
    assert_eq!(
        run_timeline(vec![0x05, 0x06]),
        vec![0x42, 0x01, 0x01, 0xFF],
        "clean schedule"
    );

    // A partition between command and ack makes the retry double-apply.
    assert_eq!(
        run_timeline(vec![0x05, 0xEE, 0x06]),
        vec![0x42, 0x01, 0x02, 0x01, 0xFF],
        "partition schedule shows the duplicate apply"
    );

    // Replay: the same schedule produces the identical marker stream.
    assert_eq!(
        run_timeline(vec![0x05, 0xEE, 0x06]),
        vec![0x42, 0x01, 0x02, 0x01, 0xFF],
        "replay is bit-for-bit identical"
    );
}

/// Branching carries state: a child resumed from a branch point sees the
/// parent's applications as already-applied. The parent's round applied
/// commands 0x05 and 0x06; the child's round replays them (both report the
/// duplicate marker) plus a fresh command (applied once).
#[test]
fn counter_branching_inherits_state() {
    let seccomp_filters = get_empty_filters();
    let mut root_evmgr = EventManager::new().unwrap();
    let explorer = Explorer::explore(
        42,
        &ExplorerConfig {
            events: vec![0x05, 0x06],
            branch_event_suffix: true,
            rendezvous: true,
            faults: None,
            run_ms: 300,
            branches_per_node: 1,
            max_depth: 1,
        },
        &InstanceInfo::default(),
        &seccomp_filters,
        &mut root_evmgr,
        |instance_info, evmgr, filters| {
            Ok(build_microvm_for_boot(instance_info, &guest_resources(), evmgr, filters)?)
        },
        &VmResources::default,
    )
    .unwrap();

    // Root: boot marker, two first-time applications, done.
    assert_eq!(
        explorer.tree.node(0).payload.as_ref().unwrap().markers,
        vec![0x42, 0x01, 0x01, 0xFF],
        "root markers"
    );

    // Child: events [0x05, 0x06, 0x01] (base + suffix 1). It resumes with
    // the parent's applied state, so 0x05 and 0x06 are duplicates and only
    // 0x01 is new.
    assert_eq!(
        explorer.tree.node(1).payload.as_ref().unwrap().markers,
        vec![0x02, 0x02, 0x01, 0xFF],
        "child inherits the parent's applied state"
    );
}
