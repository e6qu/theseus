// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tutorial driver (tutorial 04): runs the counter guest through the
//! explorer with and without a partition, prints the marker streams, and
//! proves the partition-injected retry bug and its bit-for-bit replay.
//! Run it on a Linux+KVM host (see the tutorial's README).

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
    path.push("../guest/counter_guest.bin");
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
            serial_events: Vec::new(),
            branch_event_suffix: false,
            rendezvous: true,
            faults: None,
            run_ms: 300,
            branches_per_node: 0,
            max_depth: 0,
            max_nodes: 1,
            novelty: NoveltyStrategy::Markers,
            serial_log_dir: None,
        },
        &InstanceInfo::default(),
        &seccomp_filters,
        &mut root_evmgr,
        |instance_info, evmgr, filters| {
            Ok(build_microvm_for_boot(
                instance_info,
                &guest_resources(),
                evmgr,
                filters,
            )?)
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

fn main() {
    // Two increments, no partition: each applied exactly once.
    let clean = run_timeline(vec![0x05, 0x06]);
    println!("clean schedule:     {clean:02x?}");
    assert_eq!(clean, vec![0x42, 0x01, 0x01, 0xFF], "clean schedule");

    // A partition between command and ack makes the retry double-apply.
    let partitioned = run_timeline(vec![0x05, 0xEE, 0x06]);
    println!("partition schedule: {partitioned:02x?}   <- 0x02 is the duplicate apply");
    assert_eq!(
        partitioned,
        vec![0x42, 0x01, 0x02, 0x01, 0xFF],
        "partition schedule shows the duplicate apply"
    );

    // Replay: the same schedule produces the identical marker stream.
    let replay = run_timeline(vec![0x05, 0xEE, 0x06]);
    println!("replay:             {replay:02x?}");
    assert_eq!(replay, partitioned, "replay is bit-for-bit identical");

    println!("all assertions passed");
}
