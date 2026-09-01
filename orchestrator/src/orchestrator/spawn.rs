// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Spawning child timelines from a branch point.
//!
//! A child is a full microVM restored from the branch point's in-memory state
//! (memfd + serialized `MicrovmState`), re-seeded before resume so that it
//! diverges from its parent *only* by seed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::branch::{BranchError, BranchPoint};
use vmm::devices::virtio::net::SimNetConfig;
use vmm::persist::{restore_from_microvm_state, RestoreFromSnapshotError};
use vmm::resources::VmResources;
use vmm::seccomp::BpfThreadMap;
use vmm::vmm_config::instance_info::InstanceInfo;
use vmm::vmm_config::snapshot::{
    LoadSnapshotParams, MemBackendConfig, MemBackendType, SnapshotLoadHugePageConfig,
};
use vmm::EventManager;
use vmm::Vmm;

/// Errors from spawning a child timeline.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum SpawnError {
    /// Branch point error: {0}
    Branch(#[from] BranchError),
    /// Could not restore the child microVM: {0}
    Restore(#[from] Box<RestoreFromSnapshotError>),
    /// Could not reseed the child's entropy device: {0}
    Reseed(#[from] vmm::VmmError),
}

/// A spawned child timeline.
#[derive(Debug)]
pub struct ChildVm {
    /// The child's microVM, restored but not yet resumed.
    pub vmm: Arc<Mutex<Vmm>>,
    /// The seed this child diverges with.
    pub seed: u64,
    /// Host-side deterministic stream for this child timeline.
    pub host_rng: vmm::detrng::Stream,
}

/// Spawn a child timeline from a branch point.
///
/// The child restores from the branch point's in-memory state. Its entropy
/// device is re-seeded with a fresh deterministic seed *before* the caller
/// resumes it, so parent and child observe different entropy streams — the
/// single axis of divergence.
///
/// `fault_cfg`, when set, overrides the simulated-network configuration of
/// every sim-backed net device in the captured state: fault injection is the
/// second axis of divergence (same branch point, different fault schedule).
///
/// `clock_realtime` is hard-wired to `false`: re-anchoring to host wall time
/// on restore would break replay.
pub fn spawn_child(
    branch: &mut BranchPoint,
    fault_cfg: Option<SimNetConfig>,
    instance_info: &InstanceInfo,
    event_manager: &mut EventManager,
    seccomp_filters: &BpfThreadMap,
    vm_resources: &mut VmResources,
) -> Result<ChildVm, SpawnError> {
    let seed = branch.child_seed();
    let host_rng = vmm::detrng::Stream::seeded(seed);
    let mut microvm_state = branch.microvm_state()?;

    if let Some(cfg) = fault_cfg {
        use vmm::VirtioDevicesState;
        // MMIO and PCI transport states are distinct types; apply the fault
        // config to every sim-backed net device in either.
        match &mut microvm_state.device_states.virtio_state {
            VirtioDevicesState::Mmio(mmio) => {
                for dev in &mut mmio.net_devices {
                    if dev.device_state.sim.is_some() {
                        dev.device_state.sim = Some(cfg);
                    }
                }
            }
            VirtioDevicesState::Pci(pci) => {
                for dev in &mut pci.net_devices {
                    if dev.device_state.sim.is_some() {
                        dev.device_state.sim = Some(cfg);
                    }
                }
            }
        }
    }

    let params = LoadSnapshotParams {
        // Unused by the in-memory path.
        snapshot_path: PathBuf::new(),
        mem_backend: MemBackendConfig {
            backend_type: MemBackendType::File,
            backend_path: PathBuf::from(branch.memory_fd_path()),
        },
        track_dirty_pages: false,
        resume_vm: false,
        network_overrides: Vec::new(),
        vsock_override: None,
        clock_realtime: false,
        huge_pages: SnapshotLoadHugePageConfig::Snapshot,
    };

    let vmm = vmm::detrng::with_stream(&host_rng, || {
        restore_from_microvm_state(
            instance_info,
            event_manager,
            seccomp_filters,
            microvm_state,
            &params,
            vm_resources,
        )
    })
    .map_err(Box::new)?;

    vmm.lock().expect("Poisoned lock").reseed_entropy(seed)?;

    Ok(ChildVm {
        vmm,
        seed,
        host_rng,
    })
}

#[cfg(test)]
mod tests {
    use rand_chacha::rand_core::{RngCore, SeedableRng};
    use rand_chacha::ChaCha8Rng;

    use super::*;
    use vmm::builder::build_microvm_for_boot;
    use vmm::persist::VmInfo;
    use vmm::rpc_interface::{RuntimeApiController, VmmAction};
    use vmm::seccomp::get_empty_filters;
    use vmm::test_utils::mock_resources::{MockBootSourceConfig, MockVmResources};
    use vmm::vmm_config::entropy::EntropyDeviceConfig;
    use vmm::{EventManager, FcExitCode};

    fn boot_parent(seed: u64) -> (Arc<Mutex<Vmm>>, EventManager, BpfThreadMap) {
        let boot_source_cfg = MockBootSourceConfig::new().with_default_boot_args().into();
        let mut resources: VmResources = MockVmResources::new()
            .with_boot_source(boot_source_cfg)
            .into();
        resources
            .entropy
            .insert(EntropyDeviceConfig {
                rate_limiter: None,
                seed: Some(seed),
                script: None,
            })
            .unwrap();

        let mut event_manager = EventManager::new().unwrap();
        let seccomp_filters = get_empty_filters();
        let parent = build_microvm_for_boot(
            &InstanceInfo::default(),
            &resources,
            &mut event_manager,
            &seccomp_filters,
        )
        .unwrap();
        parent.lock().unwrap().resume_vm().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        (parent, event_manager, seccomp_filters)
    }

    fn capture_branch(vmm: &Arc<Mutex<Vmm>>, evmgr: &mut EventManager, seed: u64) -> BranchPoint {
        let mut controller = RuntimeApiController::new(vmm.clone());
        controller.handle_request(VmmAction::Pause, evmgr).unwrap();
        let vm_info = VmInfo::from(&*vmm.lock().unwrap());
        let bp = BranchPoint::capture(&mut vmm.lock().unwrap(), &vm_info, seed).unwrap();
        vmm.lock().unwrap().stop(FcExitCode::Ok);
        bp
    }

    fn spawn(branch: &mut BranchPoint, seccomp_filters: &BpfThreadMap) -> (ChildVm, EventManager) {
        let mut resources = VmResources::default();
        let mut evmgr = EventManager::new().unwrap();
        let child = spawn_child(
            branch,
            None,
            &InstanceInfo::default(),
            &mut evmgr,
            seccomp_filters,
            &mut resources,
        )
        .unwrap();
        (child, evmgr)
    }

    /// The minimal multiverse, live: boot a parent, capture a branch point,
    /// spawn two children, and prove they diverge *only* by seed — each
    /// child's entropy stream equals a fresh ChaCha stream of its child seed,
    /// and both children boot and run cleanly.
    #[test]
    fn test_branch_children_diverge_only_by_seed() {
        let (parent, mut event_manager, seccomp_filters) = boot_parent(42);
        let mut branch = capture_branch(&parent, &mut event_manager, 42);

        let (child_a, mut evmgr_a) = spawn(&mut branch, &seccomp_filters);
        let (child_b, mut evmgr_b) = spawn(&mut branch, &seccomp_filters);

        // Children diverge only by seed.
        assert_ne!(child_a.seed, child_b.seed);
        let expected = |seed: u64| {
            let mut buf = [0u8; 32];
            ChaCha8Rng::seed_from_u64(seed).fill_bytes(&mut buf);
            buf.to_vec()
        };
        let a_bytes = child_a.vmm.lock().unwrap().entropy_probe(32);
        let b_bytes = child_b.vmm.lock().unwrap().entropy_probe(32);
        assert_eq!(a_bytes, expected(child_a.seed));
        assert_eq!(b_bytes, expected(child_b.seed));
        assert_ne!(a_bytes, b_bytes);

        // Both children resume and run.
        for (child, evmgr) in [(&child_a, &mut evmgr_a), (&child_b, &mut evmgr_b)] {
            child.vmm.lock().unwrap().resume_vm().unwrap();
            let _ = evmgr.run_with_timeout(200);
            child.vmm.lock().unwrap().stop(FcExitCode::Ok);
            assert_eq!(
                child.vmm.lock().unwrap().shutdown_exit_code(),
                Some(FcExitCode::Ok)
            );
        }
    }

    /// Children map the branch point's memfd MAP_PRIVATE: a write through one
    /// child's guest memory must be invisible to the other child and to the
    /// branch point itself. This is what makes siblings independent timelines
    /// with kernel CoW instead of a per-child RAM copy.
    #[test]
    fn test_branch_children_memory_is_cow() {
        use vm_memory::Bytes;
        use vmm::vstate::memory::GuestAddress;

        let (parent, mut event_manager, seccomp_filters) = boot_parent(42);
        let mut branch = capture_branch(&parent, &mut event_manager, 42);

        let (child_a, _evmgr_a) = spawn(&mut branch, &seccomp_filters);
        let (child_b, _evmgr_b) = spawn(&mut branch, &seccomp_filters);

        #[cfg(target_arch = "aarch64")]
        const DRAM_START: u64 = vmm::arch::DRAM_MEM_START;
        #[cfg(target_arch = "x86_64")]
        const DRAM_START: u64 = 0; // x86_64 guest RAM starts at 0
        let addr = GuestAddress(DRAM_START + 0x400000);
        let mem_a = child_a
            .vmm
            .lock()
            .unwrap()
            .vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .clone();
        let mem_b = child_b
            .vmm
            .lock()
            .unwrap()
            .vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .clone();

        // Same content before the write.
        let mut before_a = [0u8; 16];
        let mut before_b = [0u8; 16];
        mem_a.read_slice(&mut before_a, addr).unwrap();
        mem_b.read_slice(&mut before_b, addr).unwrap();
        assert_eq!(before_a, before_b);

        // Write through child A's mapping.
        let marker = *b"theseus-branch-a";
        mem_a.write_slice(&marker, addr).unwrap();

        // Child B and the branch point's memfd are unchanged.
        let mut after_b = [0u8; 16];
        mem_b.read_slice(&mut after_b, addr).unwrap();
        assert_eq!(
            after_b, before_b,
            "sibling observed another timeline's write"
        );

        use std::os::unix::fs::FileExt;
        let mut backing = vec![0u8; 16];
        // The dump is region-contiguous: the default single RAM region starts
        // at DRAM_MEM_START, so the file offset is the guest offset minus it.
        branch
            .memory_file()
            .read_exact_at(&mut backing, 0x400000)
            .unwrap();
        assert_eq!(
            &backing[..],
            &before_b[..],
            "branch point memfd was mutated"
        );

        child_a.vmm.lock().unwrap().stop(FcExitCode::Ok);
        child_b.vmm.lock().unwrap().stop(FcExitCode::Ok);
    }

    /// Fault injection as a branch axis: children spawned from one branch
    /// point with a fault schedule get deterministic, per-child sim-net
    /// configs. Requires KVM.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_spawn_child_with_fault_schedule() {
        use vmm::devices::virtio::net::SimNetConfig;
        use vmm::vmm_config::net::NetworkInterfaceConfig;

        // Parent with a sim-backed net device (no host tap needed).
        let boot_source_cfg = MockBootSourceConfig::new().with_default_boot_args().into();
        let mut resources: VmResources = MockVmResources::new()
            .with_boot_source(boot_source_cfg)
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
            .net_builder
            .build(NetworkInterfaceConfig {
                iface_id: "net0".to_string(),
                host_dev_name: "ignored-with-sim".to_string(),
                guest_mac: None,
                mtu: None,
                rx_rate_limiter: None,
                tx_rate_limiter: None,
                sim: Some(SimNetConfig::default()),
            })
            .unwrap();

        let mut event_manager = EventManager::new().unwrap();
        let seccomp_filters = get_empty_filters();
        let parent = build_microvm_for_boot(
            &InstanceInfo::default(),
            &resources,
            &mut event_manager,
            &seccomp_filters,
        )
        .unwrap();
        parent.lock().unwrap().resume_vm().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut branch = capture_branch(&parent, &mut event_manager, 42);

        // Three children: drop_ppm steps 0, 100k, 200k; third is partitioned.
        let sim_of = |child: &ChildVm| {
            child.vmm.lock().unwrap().full_config().network_interfaces[0]
                .sim
                .unwrap()
        };
        let mut children = Vec::new();
        for idx in 0..3usize {
            let (child, _evmgr) = {
                let mut resources = VmResources::default();
                let mut evmgr = EventManager::new().unwrap();
                let cfg = SimNetConfig {
                    seed: 0,
                    loopback: true,
                    drop_ppm: 100_000 * idx as u32,
                    partitioned: idx == 2,
                };
                let child = spawn_child(
                    &mut branch,
                    Some(cfg),
                    &InstanceInfo::default(),
                    &mut evmgr,
                    &seccomp_filters,
                    &mut resources,
                )
                .unwrap();
                (child, evmgr)
            };
            children.push(child);
        }

        for (idx, child) in children.iter().enumerate() {
            let sim = sim_of(child);
            assert_eq!(sim.drop_ppm, 100_000 * idx as u32, "child {idx} drop_ppm");
            assert_eq!(sim.partitioned, idx == 2, "child {idx} partitioned");
        }

        for child in &children {
            child.vmm.lock().unwrap().stop(FcExitCode::Ok);
        }
    }

    /// A live bare-metal guest talks to the control channel: the guest reads
    /// the magic register, issues setup-complete and a log marker over MMIO,
    /// and the host drains exactly those events. Requires KVM.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_guest_control_channel_roundtrip() {
        use vmm::devices::pseudo::ControlEvent;
        use vmm::test_utils::mock_resources::kernel_image_path;
        use vmm::vmm_config::boot_source::BootSourceConfig;

        let resources: VmResources = MockVmResources::new()
            .with_boot_source(BootSourceConfig {
                kernel_image_path: kernel_image_path(Some("theseus_guest.bin")),
                boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
                initrd_path: None,
            })
            .into();

        let mut event_manager = EventManager::new().unwrap();
        let seccomp_filters = get_empty_filters();
        let vmm = build_microvm_for_boot(
            &InstanceInfo::default(),
            &resources,
            &mut event_manager,
            &seccomp_filters,
        )
        .unwrap();
        vmm.lock().unwrap().resume_vm().unwrap();
        let _ = event_manager.run_with_timeout(500);

        let events = vmm.lock().unwrap().drain_control_events();
        assert_eq!(
            events,
            // The guest logs its boot marker, signals setup-complete, then
            // waits for events (none pushed in this test — it spins, and we
            // stop the VM below).
            vec![ControlEvent::GuestLog(0x42), ControlEvent::SetupComplete],
            "guest did not issue the expected control-channel sequence"
        );

        vmm.lock().unwrap().stop(FcExitCode::Ok);
    }

    /// Track B′ on metal (aarch64): with virtual time enabled, a guest reading
    /// CNTVCT sees time that is a pure function of the tick count — identical
    /// across runs. With it disabled, the guest sees the host counter, which
    /// differs across runs.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_guest_virtual_time_is_reproducible() {
        use vmm::test_utils::mock_resources::kernel_image_path;
        use vmm::vmm_config::boot_source::BootSourceConfig;
        use vmm::vmm_config::machine_config::{MachineConfigUpdate, VirtualTimeConfig};

        fn boot_and_read_vtime(virtual_time: Option<VirtualTimeConfig>) -> String {
            let serial = vmm_sys_util::tempfile::TempFile::new().unwrap();
            let serial_path = serial.as_path().to_path_buf();

            let mut resources: VmResources = MockVmResources::new()
                .with_boot_source(BootSourceConfig {
                    kernel_image_path: kernel_image_path(Some("theseus_guest.bin")),
                    boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
                    initrd_path: None,
                })
                .into();
            resources.serial_out_path = Some(serial_path.clone());
            if let Some(vt) = virtual_time {
                resources
                    .update_machine_config(&MachineConfigUpdate {
                        virtual_time: Some(vt),
                        ..Default::default()
                    })
                    .unwrap();
            }

            let mut event_manager = EventManager::new().unwrap();
            let seccomp_filters = get_empty_filters();
            let vmm = build_microvm_for_boot(
                &InstanceInfo::default(),
                &resources,
                &mut event_manager,
                &seccomp_filters,
            )
            .unwrap();
            vmm.lock().unwrap().resume_vm().unwrap();
            let _ = event_manager.run_with_timeout(500);
            vmm.lock().unwrap().stop(FcExitCode::Ok);

            let log = std::fs::read_to_string(&serial_path).unwrap();
            let marker = "vtime=0x";
            let start = log.find(marker).expect("guest did not print vtime") + marker.len();
            log[start..start + 16].to_string()
        }

        let vt = Some(VirtualTimeConfig {
            tick_ns: 1_000_000,
            exits_per_tick: 64,
        });
        let vt_run1 = boot_and_read_vtime(vt);
        let vt_run2 = boot_and_read_vtime(vt);

        // The B′ contract, asserted honestly: anchored (near zero, not the
        // host's ~10^12 counter) and bounded-closeness — a mid-quantum CNTVCT
        // read carries the free-run tail plus host preemption slack, so two
        // runs agree only within a few ticks. Bitwise identity needs Track B
        // (counter-read trapping), which the plan parks deliberately.
        let parse = |s: &str| u64::from_str_radix(s, 16).unwrap();
        let (v1, v2) = (parse(&vt_run1), parse(&vt_run2));
        const ANCHOR_BOUND: u64 = 1_000_000; // counts; ~42ms @24MHz, 1ms @1GHz
        assert!(
            v1 < ANCHOR_BOUND && v2 < ANCHOR_BOUND,
            "virtual time not anchored near zero: {v1:#x}, {v2:#x}"
        );
        assert!(
            v1.abs_diff(v2) < ANCHOR_BOUND,
            "virtual time diverged beyond bound: {v1:#x} vs {v2:#x}"
        );

        // Control: without virtual time, the guest sees the host counter
        // (KVM anchors it at vCPU init, so values are small but NOT
        // reproducible — they measure host boot→read duration).
        let host_run1 = boot_and_read_vtime(None);
        let host_run2 = boot_and_read_vtime(None);
        assert_ne!(
            host_run1, host_run2,
            "host counter should differ across runs (got {host_run1} twice)"
        );
    }

    /// The Rust guest (theseus-sdk) event paths, driven directly: push
    /// [0x90, terminator] before boot; the guest must mark the high path
    /// (0xB0), echo 0x90, and finish (0xFF). Requires KVM; aarch64.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_rust_guest_event_paths() {
        use vmm::devices::pseudo::ControlEvent;
        use vmm::test_utils::mock_resources::kernel_image_path;
        use vmm::vmm_config::boot_source::BootSourceConfig;

        let resources: VmResources = MockVmResources::new()
            .with_boot_source(BootSourceConfig {
                kernel_image_path: kernel_image_path(Some("theseus_guest_rs.bin")),
                boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
                initrd_path: None,
            })
            .into();

        let mut event_manager = EventManager::new().unwrap();
        let seccomp_filters = get_empty_filters();
        let vmm = build_microvm_for_boot(
            &InstanceInfo::default(),
            &resources,
            &mut event_manager,
            &seccomp_filters,
        )
        .unwrap();

        // Push the round before the guest reaches its event loop (the FIFO
        // buffers them).
        {
            let mut vmm = vmm.lock().unwrap();
            vmm.push_control_event(0x90).unwrap();
            vmm.push_control_event(0x00).unwrap();
        }
        vmm.lock().unwrap().resume_vm().unwrap();
        let _ = event_manager.run_with_timeout(500);

        let events = vmm.lock().unwrap().drain_control_events();
        assert_eq!(
            events,
            vec![
                ControlEvent::GuestLog(0x42), // boot
                ControlEvent::SetupComplete,
                ControlEvent::GuestLog(0xB0), // high path for 0x90
                ControlEvent::GuestLog(0x90), // echo
                ControlEvent::GuestLog(0xFF), // done
            ],
            "rust guest marker stream wrong: {events:?}"
        );

        vmm.lock().unwrap().stop(FcExitCode::Ok);
    }
}
