// Copyright 2026 Theseus contributors.
// SPDX-License-Identifier: Apache-2.0

//! The live exploration loop: timelines as a running system.
//!
//! `Explorer` drives the machine: boot a root timeline, run it, push
//! control-channel events, pause, capture a branch point, then spawn children
//! that diverge only by seed — recursively, in the tree's deterministic DFS
//! order.
//!
//! Every node records an **entropy probe** — the next bytes its entropy
//! device would serve, captured at pause time. Two runs of the same
//! exploration must produce identical probes at every node: that is the
//! replay property at loop level, checked continuously.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::branch::{BranchError, BranchPoint};
use crate::orchestrator::spawn::{SpawnError, spawn_child};
use crate::orchestrator::tree::{NodeId, TimelineTree};
use crate::persist::VmInfo;
use crate::resources::VmResources;
use crate::seccomp::BpfThreadMap;
use crate::vmm_config::instance_info::InstanceInfo;
use crate::{EventManager, FcExitCode, Vmm, VmmError};

/// Errors from the exploration loop.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum ExplorerError {
    /// Branch point error: {0}
    Branch(#[from] BranchError),
    /// Child spawn error: {0}
    Spawn(#[from] SpawnError),
    /// Vmm error: {0}
    Vmm(#[from] VmmError),
    /// Could not build the root microVM: {0}
    Build(#[from] crate::builder::StartMicrovmError),
    /// Timed out waiting for a rendezvous marker from the guest. Carries the
    /// events collected while waiting (debugging rendezvous failures without
    /// a guest console).
    RendezvousTimeout(&'static str, String),
}

/// Deterministic fault schedule: which simulated-network faults each child
/// gets, as a pure function of its branch index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultStrategy {
    /// `drop_ppm` for a child: base + step * branch_index.
    pub drop_ppm_base: u32,
    /// Step per branch index.
    pub drop_ppm_step: u32,
    /// Every Nth child (branch_index % n == n-1) spawns partitioned. 0 = never.
    pub partition_every_n: u32,
}

impl FaultStrategy {
    /// The sim-net config for the child at `branch_idx`.
    pub fn sim_config(&self, branch_idx: usize) -> crate::devices::virtio::net::SimNetConfig {
        crate::devices::virtio::net::SimNetConfig {
            seed: 0,
            loopback: true,
            drop_ppm: self.drop_ppm_base + self.drop_ppm_step * branch_idx as u32,
            partitioned: self.partition_every_n != 0
                && branch_idx as u32 % self.partition_every_n == self.partition_every_n - 1,
        }
    }
}

/// Deterministic exploration parameters.
#[derive(Debug, Clone)]
pub struct ExplorerConfig {
    /// Event bytes pushed into each timeline's control-channel FIFO (same
    /// base for every node — variance comes from seeds).
    pub events: Vec<u8>,
    /// Append the branch index + 1 as a final event byte for children (gives
    /// siblings distinguishable input schedules, deterministically; 0x00 is
    /// the protocol's terminator).
    pub branch_event_suffix: bool,
    /// Rendezvous mode: the workload signals `SetupComplete` through the
    /// control channel, then the explorer pushes events + a 0x00 terminator
    /// and waits for the `0xFF` done marker before pausing. Required for
    /// reactive workloads; off for guests that ignore the channel.
    pub rendezvous: bool,
    /// Per-child fault schedule for sim-backed net devices (the second axis
    /// of divergence after seeds).
    pub faults: Option<FaultStrategy>,
    /// How long each timeline runs before capture, in milliseconds.
    pub run_ms: u64,
    /// Children spawned per branch point.
    pub branches_per_node: usize,
    /// Maximum tree depth (root = 0).
    pub max_depth: u32,
}

/// A node's payload: the captured branch point, plus the fingerprints
/// recorded when the timeline was paused.
#[derive(Debug)]
pub struct ExploredNode {
    /// Captured microVM state from which children can be spawned.
    pub branch_point: BranchPoint,
    /// The next bytes the timeline's entropy device would serve at capture
    /// time. A run fingerprint: replays must reproduce it exactly.
    pub entropy_probe: Vec<u8>,
    /// Guest log markers drained at capture time — the v1 coverage signal.
    pub markers: Vec<u8>,
    /// Dirty guest pages at capture time, if dirty tracking is enabled — a
    /// memory-footprint coverage signal. Deterministic timelines dirty the
    /// same pages.
    pub dirty_pages: Option<u64>,
}

/// The exploration engine. Owns the timeline tree.
#[derive(Debug)]
pub struct Explorer {
    /// The timeline tree built so far.
    pub tree: TimelineTree<ExploredNode>,
}

/// Bytes probed from each timeline's entropy device at capture time.
const ENTROPY_PROBE_LEN: usize = 32;

impl Explorer {
    /// Run the loop. `build_root` constructs (but does not boot) the root
    /// microVM, registering its devices into `root_evmgr`.
    ///
    /// Requires KVM. Single-threaded by design: determinism first;
    /// scale-out is one process per timeline.
    pub fn explore<F>(
        root_seed: u64,
        config: &ExplorerConfig,
        instance_info: &InstanceInfo,
        seccomp_filters: &BpfThreadMap,
        root_evmgr: &mut EventManager,
        build_root: impl FnOnce(
            &InstanceInfo,
            &mut EventManager,
            &BpfThreadMap,
        ) -> Result<Arc<Mutex<Vmm>>, ExplorerError>,
        child_resources: &F,
    ) -> Result<Self, ExplorerError>
    where
        F: Fn() -> VmResources,
    {
        let root_vmm = build_root(instance_info, root_evmgr, seccomp_filters)?;
        let root_node =
            Self::run_and_capture(root_vmm, &config.events, config, root_seed, true)?;

        let mut explorer = Explorer {
            tree: TimelineTree::new(root_seed, root_node),
        };
        explorer.expand(0, config, instance_info, seccomp_filters, child_resources)?;
        Ok(explorer)
    }

    /// Run one timeline, headless (no EventManager pumping): the vCPU thread
    /// handles MMIO synchronously, and pause/probe/capture are Vmm methods.
    /// This is what makes children free to run on their own threads.
    ///
    /// Constraint: the timeline must not use host-fd-backed devices (tap
    /// networks, file-backed blocks) — those need an EventManager pumping
    /// their EventFds. Sim backends and the MMIO control channel are
    /// pump-free by construction.
    ///
    /// Two modes:
    /// - non-reactive: push events into the FIFO, then resume and run.
    /// - rendezvous: resume, [root only: wait for `SetupComplete`], push
    ///   events + a 0x00 terminator, wait for the `0xFF` done marker.
    ///   Children skip the setup wait: a branch continues from the pause
    ///   point (the guest parked in its event loop), it does not replay
    ///   earlier guest code.
    fn run_and_capture(
        vmm: Arc<Mutex<Vmm>>,
        events: &[u8],
        config: &ExplorerConfig,
        seed: u64,
        await_setup: bool,
    ) -> Result<ExploredNode, ExplorerError> {
        let mut collected: Vec<crate::devices::pseudo::ControlEvent> = Vec::new();

        if config.rendezvous {
            vmm.lock().expect("Poisoned lock").resume_vm()?;
            if await_setup {
                Self::wait_for_marker(
                    &vmm,
                    &mut collected,
                    |ev| *ev == crate::devices::pseudo::ControlEvent::SetupComplete,
                    "setup-complete",
                )?;
            }
            {
                let mut vmm = vmm.lock().expect("Poisoned lock");
                for &byte in events {
                    vmm.push_control_event(byte)?;
                }
                vmm.push_control_event(0x00)?; // terminator
            }
            Self::wait_for_marker(
                &vmm,
                &mut collected,
                |ev| *ev == crate::devices::pseudo::ControlEvent::GuestLog(0xFF),
                "done",
            )?;
        } else {
            {
                let mut vmm = vmm.lock().expect("Poisoned lock");
                for &byte in events {
                    vmm.push_control_event(byte)?;
                }
                vmm.resume_vm()?;
            }
            thread::sleep(Duration::from_millis(config.run_ms));
        }

        vmm.lock().expect("Poisoned lock").pause_vm()?;

        let (entropy_probe, markers, dirty_pages) = {
            let mut vmm = vmm.lock().expect("Poisoned lock");
            collected.extend(vmm.drain_control_events());
            let probe = vmm.entropy_probe(ENTROPY_PROBE_LEN);
            let dirty = vmm.dirty_page_count();
            let markers = collected
                .iter()
                .filter_map(|ev| match ev {
                    crate::devices::pseudo::ControlEvent::GuestLog(byte) => Some(*byte),
                    _ => None,
                })
                .collect();
            (probe, markers, dirty)
        };
        let vm_info = VmInfo::from(&*vmm.lock().expect("Poisoned lock"));
        let branch_point =
            BranchPoint::capture(&mut vmm.lock().expect("Poisoned lock"), &vm_info, seed)?;
        vmm.lock().expect("Poisoned lock").stop(FcExitCode::Ok);
        Ok(ExploredNode {
            branch_point,
            entropy_probe,
            markers,
            dirty_pages,
        })
    }

    /// Drain control-channel events until `pred` matches one of them.
    /// Bounded by ~2s of host time.
    fn wait_for_marker(
        vmm: &Arc<Mutex<Vmm>>,
        collected: &mut Vec<crate::devices::pseudo::ControlEvent>,
        pred: impl Fn(&crate::devices::pseudo::ControlEvent) -> bool,
        what: &'static str,
    ) -> Result<(), ExplorerError> {
        if collected.iter().any(&pred) {
            return Ok(());
        }
        for _ in 0..200 {
            thread::sleep(Duration::from_millis(10));
            let events = vmm.lock().expect("Poisoned lock").drain_control_events();
            let found = events.iter().any(&pred);
            collected.extend(events);
            if found {
                return Ok(());
            }
        }
        Err(ExplorerError::RendezvousTimeout(
            what,
            format!("{collected:?}"),
        ))
    }

    /// Depth-first expansion: spawn `branches_per_node` children from the
    /// node's branch point, run each, capture their branch points.
    ///
    /// **Novelty-guided**: children whose marker sets contain bytes not yet
    /// seen anywhere in the tree are expanded first (deterministic tie-break:
    /// seed order). This is the v1 coverage signal — guest log markers as
    /// observable behavior; genuine code coverage needs guest instrumentation.
    fn expand<F>(
        &mut self,
        node: NodeId,
        config: &ExplorerConfig,
        instance_info: &InstanceInfo,
        seccomp_filters: &BpfThreadMap,
        child_resources: &F,
    ) -> Result<(), ExplorerError>
    where
        F: Fn() -> VmResources,
    {
        if self.tree.node(node).depth >= config.max_depth {
            return Ok(());
        }

        // Spawn children sequentially (each takes &mut from the branch
        // point), then run them on their own threads — one timeline per
        // thread, results joined in spawn order so the tree is deterministic.
        let mut spawned = Vec::new();
        for branch_idx in 0..config.branches_per_node {
            let mut resources = child_resources();
            let mut evmgr = EventManager::new().unwrap();
            let child = spawn_child(
                &mut self.tree.payload_mut(node).branch_point,
                config.faults.map(|f| f.sim_config(branch_idx)),
                instance_info,
                &mut evmgr,
                seccomp_filters,
                &mut resources,
            )?;
            let mut events = config.events.clone();
            if config.branch_event_suffix {
                // +1: 0x00 is the protocol's terminator byte.
                events.push(branch_idx as u8 + 1);
            }
            spawned.push((child, events));
        }

        let results: Vec<Result<(u64, ExploredNode), ExplorerError>> = thread::scope(|scope| {
            let handles: Vec<_> = spawned
                .into_iter()
                .map(|(child, events)| {
                    scope.spawn(move || {
                        Self::run_and_capture(child.vmm, &events, config, child.seed, false)
                            .map(|node| (child.seed, node))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("timeline thread panicked"))
                .collect()
        });

        let mut children: Vec<(NodeId, Vec<u8>)> = Vec::new();
        for result in results {
            let (seed, explored) = result?;
            let markers = explored.markers.clone();
            children.push((self.tree.add_child(node, seed, explored), markers));
        }

        // Deterministic novelty ordering: most-novel markers first, then by
        // seed. Computed against the marker set seen before this expansion.
        let mut seen: std::collections::BTreeSet<u8> = std::collections::BTreeSet::new();
        for id in self.tree.exploration_order() {
            if let Some(payload) = self.tree.node(id).payload.as_ref() {
                seen.extend(payload.markers.iter().copied());
            }
        }
        children.sort_by_key(|(id, markers)| {
            let novelty = markers.iter().filter(|m| !seen.contains(m)).count();
            (
                std::cmp::Reverse(novelty),
                self.tree.node(*id).seed,
            )
        });

        for (child_id, _) in children {
            self.expand(child_id, config, instance_info, seccomp_filters, child_resources)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rand_chacha::ChaCha8Rng;
    use rand_chacha::rand_core::{RngCore, SeedableRng};

    use super::*;
    use crate::builder::build_microvm_for_boot;
    use crate::seccomp::get_empty_filters;
    use crate::test_utils::mock_resources::{MockBootSourceConfig, MockVmResources};
    use crate::vmm_config::entropy::EntropyDeviceConfig;

    fn root_resources() -> VmResources {
        let boot_source_cfg = MockBootSourceConfig::new()
            .with_default_boot_args()
            .into();
        let mut resources: VmResources = MockVmResources::new()
            .with_boot_source(boot_source_cfg)
            .with_vm_config(
                crate::test_utils::mock_resources::MockVmConfig::new()
                    .with_dirty_page_tracking()
                    .into(),
            )
            .into();
        resources
            .entropy
            .insert(EntropyDeviceConfig {
                rate_limiter: None,
                seed: Some(42),
            })
            .unwrap();
        resources
    }

    /// The live loop, twice: same config must produce the same tree — same
    /// shape, same seeds, same entropy probes, same dirty-page footprints at
    /// every node. Requires KVM.
    #[test]
    fn test_explore_is_deterministic() {
        let config = ExplorerConfig {
            events: vec![0x01, 0x02, 0x03],
            branch_event_suffix: false,
            faults: None,
            rendezvous: false,
            run_ms: 150,
            branches_per_node: 2,
            max_depth: 1,
        };
        let seccomp_filters = get_empty_filters();

        let run = || {
            let mut root_evmgr = EventManager::new().unwrap();
            Explorer::explore(
                42,
                &config,
                &InstanceInfo::default(),
                &seccomp_filters,
                &mut root_evmgr,
                |instance_info, evmgr, filters| {
                    Ok(build_microvm_for_boot(
                        instance_info,
                        &root_resources(),
                        evmgr,
                        filters,
                    )?)
                },
                &VmResources::default,
            )
            .unwrap()
        };

        let exp_a = run();
        let exp_b = run();

        // Same shape: root + 2 children, same exploration order.
        assert_eq!(exp_a.tree.len(), 3);
        assert_eq!(exp_b.tree.len(), 3);
        assert_eq!(exp_a.tree.exploration_order(), exp_b.tree.exploration_order());

        for id in 0..exp_a.tree.len() as NodeId {
            let node_a = exp_a.tree.node(id);
            let node_b = exp_b.tree.node(id);
            assert_eq!(node_a.seed, node_b.seed, "seed mismatch at node {id}");
            assert_eq!(
                node_a.payload.as_ref().unwrap().entropy_probe,
                node_b.payload.as_ref().unwrap().entropy_probe,
                "entropy probe mismatch at node {id} — replay is broken"
            );
            assert_eq!(
                node_a.payload.as_ref().unwrap().dirty_pages,
                node_b.payload.as_ref().unwrap().dirty_pages,
                "dirty-page footprint mismatch at node {id} — replay is broken"
            );
        }

        // Children diverge from the root and each other by seed: each child's
        // probe equals a fresh ChaCha stream of its own seed.
        let root_probe = &exp_a.tree.node(0).payload.as_ref().unwrap().entropy_probe;
        for id in 1..exp_a.tree.len() as NodeId {
            let node = exp_a.tree.node(id);
            let mut expected = [0u8; ENTROPY_PROBE_LEN];
            ChaCha8Rng::seed_from_u64(node.seed).fill_bytes(&mut expected);
            assert_eq!(
                node.payload.as_ref().unwrap().entropy_probe,
                expected.to_vec(),
                "child {id} entropy is not a fresh stream of its seed"
            );
            assert_ne!(
                &node.payload.as_ref().unwrap().entropy_probe,
                root_probe,
                "child {id} did not diverge from root"
            );
        }
    }

    /// The loop against a *reactive* guest (the bare-metal Theseus guest,
    /// which echoes control-channel events back as log markers): markers are
    /// the coverage signal — root echoes the base events, each child echoes
    /// the base events plus its branch-index suffix. Deterministic across
    /// runs. Requires KVM; aarch64 (the test guest is an arm64 Image).
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_explore_with_reactive_guest() {
        use crate::test_utils::mock_resources::kernel_image_path;
        use crate::vmm_config::boot_source::BootSourceConfig;

        fn reactive_resources() -> VmResources {
            let mut resources: VmResources = MockVmResources::new()
                .with_boot_source(BootSourceConfig {
                    kernel_image_path: kernel_image_path(Some("theseus_guest.bin")),
                    boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
                    initrd_path: None,
                })
                .into();
            resources
                .entropy
                .insert(EntropyDeviceConfig {
                    rate_limiter: None,
                    seed: Some(42),
                })
                .unwrap();
            resources
        }

        let config = ExplorerConfig {
            events: vec![0xA0, 0xA1],
            branch_event_suffix: true,
            rendezvous: true,
            faults: None,
            run_ms: 300,
            branches_per_node: 2,
            max_depth: 1,
        };
        let seccomp_filters = get_empty_filters();

        let run = || {
            let mut root_evmgr = EventManager::new().unwrap();
            Explorer::explore(
                42,
                &config,
                &InstanceInfo::default(),
                &seccomp_filters,
                &mut root_evmgr,
                |instance_info, evmgr, filters| {
                    Ok(build_microvm_for_boot(
                        instance_info,
                        &reactive_resources(),
                        evmgr,
                        filters,
                    )?)
                },
                &VmResources::default,
            )
            .unwrap()
        };

        let exp_a = run();
        let exp_b = run();

        // Root echoes the base events: boot marker (0x42), the echoed event
        // bytes, done marker (0xFF).
        let root_markers = &exp_a.tree.node(0).payload.as_ref().unwrap().markers;
        assert_eq!(root_markers, &vec![0x42, 0xA0, 0xA1, 0xFF], "root markers");

        // Children resume into the guest's event loop (past boot): their
        // markers are the echoed events + branch-index suffix + done marker.
        // (Suffixes are branch_idx + 1; 0x00 is the terminator.)
        for (id, suffix) in [(1u64, 1u8), (2, 2)] {
            let markers = &exp_a.tree.node(id).payload.as_ref().unwrap().markers;
            assert_eq!(
                markers,
                &vec![0xA0, 0xA1, suffix, 0xFF],
                "child {id} markers"
            );
        }

        // Full-run determinism: shape, order, seeds, probes, markers.
        assert_eq!(exp_a.tree.len(), exp_b.tree.len());
        assert_eq!(exp_a.tree.exploration_order(), exp_b.tree.exploration_order());
        for id in 0..exp_a.tree.len() as NodeId {
            let (a, b) = (exp_a.tree.node(id), exp_b.tree.node(id));
            assert_eq!(a.seed, b.seed);
            assert_eq!(
                a.payload.as_ref().unwrap().entropy_probe,
                b.payload.as_ref().unwrap().entropy_probe
            );
            assert_eq!(
                a.payload.as_ref().unwrap().markers,
                b.payload.as_ref().unwrap().markers
            );
        }
    }

    /// The loop against the Rust bare-metal guest (built on theseus-sdk):
    /// events take distinguishable code paths — a high event (>= 0x80) marks
    /// 0xB0, a low event marks 0x50 — so marker streams diverge per input
    /// schedule. Requires KVM; aarch64.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_explore_with_rust_guest() {
        use crate::test_utils::mock_resources::kernel_image_path;
        use crate::vmm_config::boot_source::BootSourceConfig;

        fn rust_guest_resources() -> VmResources {
            let mut resources: VmResources = MockVmResources::new()
                .with_boot_source(BootSourceConfig {
                    kernel_image_path: kernel_image_path(Some("theseus_guest_rs.bin")),
                    boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
                    initrd_path: None,
                })
                .into();
            resources
                .entropy
                .insert(EntropyDeviceConfig {
                    rate_limiter: None,
                    seed: Some(42),
                })
                .unwrap();
            resources
        }

        let config = ExplorerConfig {
            // Base event takes the high path; child suffixes (1, 2) the low.
            events: vec![0x90],
            branch_event_suffix: true,
            rendezvous: true,
            faults: None,
            run_ms: 300,
            branches_per_node: 2,
            max_depth: 1,
        };
        let seccomp_filters = get_empty_filters();

        let mut root_evmgr = EventManager::new().unwrap();
        let explorer = Explorer::explore(
            42,
            &config,
            &InstanceInfo::default(),
            &seccomp_filters,
            &mut root_evmgr,
            |instance_info, evmgr, filters| {
                Ok(build_microvm_for_boot(
                    instance_info,
                    &rust_guest_resources(),
                    evmgr,
                    filters,
                )?)
            },
            &VmResources::default,
        )
        .unwrap();

        // Root: boot marker, high path + echo, done.
        let root_markers = &explorer.tree.node(0).payload.as_ref().unwrap().markers;
        assert_eq!(root_markers, &vec![0x42, 0xB0, 0x90, 0xFF], "root markers");

        // Children resume into the event loop: high path for the base event,
        // low path for their suffix.
        for (id, suffix) in [(1u64, 1u8), (2, 2)] {
            let markers = &explorer.tree.node(id).payload.as_ref().unwrap().markers;
            assert_eq!(
                markers,
                &vec![0xB0, 0x90, 0x50, suffix, 0xFF],
                "child {id} markers"
            );
        }
    }
}
