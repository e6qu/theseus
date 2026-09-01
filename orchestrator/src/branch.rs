// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Timeline branching — the minimal multiverse.
//!
//! A [`BranchPoint`] captures a paused microVM's complete state **in memory**:
//! the serialized [`MicrovmState`] (small) and a full guest-RAM dump in a
//! `memfd` (no disk I/O). Children are spawned through the regular snapshot
//! restore path, with the memfd referenced as `/proc/self/fd/<n>` and the
//! memory backend type `File` — so no new restore machinery is needed.
//!
//! Two children spawned from the same [`BranchPoint`] differ *only* by seed
//! (see [`BranchPoint::child_seed`]); virtual time, clock state and memory
//! contents are identical. This is the Antithesis branch-point semantics.
//!
//! v1 copies guest RAM eagerly per branch point. The copy-on-write
//! optimization (uffd write-protect, share pages until written) is the
//! follow-up; the interface here is designed to make that a transparent
//! change.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use memfd::MemfdOptions;

use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};
use vmm::persist::{MicrovmState, VmInfo};
use vmm::snapshot::Snapshot;
use vmm::vstate::memory::{GuestMemoryExtension, GuestMemoryMmap};
use vmm::Vmm;

/// Errors from branch-point capture.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BranchError {
    /// Could not save microVM state: {0}
    SaveState(#[from] vmm::persist::MicrovmStateError),
    /// Could not serialize microVM state: {0}
    Serialize(#[from] vmm::snapshot::SnapshotError),
    /// Could not deserialize microVM state: {0}
    Deserialize(vmm::snapshot::SnapshotError),
    /// Guest memory I/O error: {0}
    MemoryIo(#[from] io::Error),
    /// Could not dump guest memory: {0}
    MemoryDump(#[from] vmm::vstate::memory::MemoryError),
    /// Could not create memfd: {0}
    Memfd(memfd::Error),
}

/// SplitMix64 — deterministic child-seed derivation. Same base seed and same
/// branch index always produce the same child seed; different indices are
/// well-mixed.
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A captured microVM state from which any number of child timelines can be
/// spawned.
#[derive(Debug)]
pub struct BranchPoint {
    state_bytes: Vec<u8>,
    memory: File,
    mem_size: u64,
    base_seed: u64,
    branch_count: u64,
}

impl BranchPoint {
    /// Capture the current state of a microVM.
    ///
    /// **Precondition: all vCPUs must be paused.** Callers use the regular
    /// pause machinery; the quantum boundaries of Track B′ make pause points
    /// deterministic.
    pub fn capture(vmm: &mut Vmm, vm_info: &VmInfo, base_seed: u64) -> Result<Self, BranchError> {
        let state_bytes = serialize_state(&vmm.save_state(vm_info)?)?;

        let kvm_vm = vmm.vm.as_kvm().ok_or_else(|| {
            BranchError::SaveState(vmm::persist::MicrovmStateError::NotAllowed(
                "branching requires KVM".into(),
            ))
        })?;
        let (memory, mem_size) = dump_memory_to_memfd(kvm_vm.guest_memory())?;

        Ok(BranchPoint {
            state_bytes,
            memory,
            mem_size,
            base_seed,
            branch_count: 0,
        })
    }

    /// The seed for the next child timeline. Each call returns a fresh,
    /// deterministic value.
    pub fn child_seed(&mut self) -> u64 {
        let seed = self.child_seed_at(self.branch_count);
        self.branch_count += 1;
        seed
    }

    /// The deterministic seed for `branch_index`, without consuming a child
    /// slot. Targeted replay uses this to restore only one recorded path.
    pub fn child_seed_at(&self, branch_index: u64) -> u64 {
        splitmix64(self.base_seed ^ (branch_index << 32))
    }

    /// Deserialize the captured microVM state.
    pub fn microvm_state(&self) -> Result<MicrovmState, BranchError> {
        deserialize_state(&self.state_bytes)
    }

    /// The memory backing file. Children reference it as
    /// `/proc/self/fd/<n>` with `MemBackendType::File`.
    pub fn memory_file(&self) -> &File {
        &self.memory
    }

    /// `/proc/self/fd/<n>` path for the memory file.
    pub fn memory_fd_path(&self) -> String {
        format!("/proc/self/fd/{}", self.memory.as_raw_fd())
    }

    /// Total guest RAM size in bytes.
    pub fn mem_size(&self) -> u64 {
        self.mem_size
    }

    /// How many children have been spawned from this point.
    pub fn branch_count(&self) -> u64 {
        self.branch_count
    }

    /// Write this captured state in Firecracker's snapshot-file layout.
    ///
    /// `state_path` receives the serialized [`MicrovmState`] and
    /// `memory_path` receives the contiguous guest-memory image. Both files
    /// are independent of the in-memory branch point after this returns.
    pub fn export_snapshot(
        &self,
        state_path: &Path,
        memory_path: &Path,
    ) -> Result<(), BranchError> {
        std::fs::write(state_path, &self.state_bytes)?;
        let mut destination = File::create(memory_path)?;
        destination.set_len(self.mem_size)?;

        let mut offset = 0;
        let mut buffer = [0u8; 64 * 1024];
        while offset < self.mem_size {
            let remaining = (self.mem_size - offset) as usize;
            let chunk_len = remaining.min(buffer.len());
            let count = self.memory.read_at(&mut buffer[..chunk_len], offset)?;
            if count == 0 {
                return Err(BranchError::MemoryIo(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "branch memory ended before its recorded size",
                )));
            }
            destination.write_all(&buffer[..count])?;
            offset += count as u64;
        }
        destination.flush()?;
        Ok(())
    }
}

/// Serialize a [`MicrovmState`] to bytes (snapshot format with CRC).
pub fn serialize_state(state: &MicrovmState) -> Result<Vec<u8>, BranchError> {
    let mut buf = Vec::new();
    Snapshot::new(state).save(&mut buf)?;
    Ok(buf)
}

/// Deserialize a [`MicrovmState`] from bytes produced by [`serialize_state`].
pub fn deserialize_state(bytes: &[u8]) -> Result<MicrovmState, BranchError> {
    Snapshot::load(&mut &bytes[..])
        .map(|snapshot| snapshot.data)
        .map_err(BranchError::Deserialize)
}

/// Dump all guest memory regions, contiguously, into a fresh memfd.
///
/// The layout matches the snapshot memory-file format (regions concatenated
/// in order), so the regular snapshot-restore path can consume it directly.
pub fn dump_memory_to_memfd(mem: &GuestMemoryMmap) -> Result<(File, u64), BranchError> {
    let memfd: File = MemfdOptions::default()
        .close_on_exec(false)
        .create("theseus-branch-mem")
        .map_err(BranchError::Memfd)?
        .into_file();

    let mem_size: u64 = mem.iter().map(|region| region.len() as u64).sum();
    memfd.set_len(mem_size)?;

    let mut file = memfd;
    mem.dump(&mut file)?;
    file.flush()?;
    Ok((file, mem_size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::Bytes;
    use vmm::test_utils::single_region_mem;
    use vmm::vstate::memory::GuestAddress;

    #[test]
    fn test_state_serialization_roundtrip() {
        let state = MicrovmState::default();
        let bytes = serialize_state(&state).unwrap();
        let restored = deserialize_state(&bytes).unwrap();
        // MicrovmState has no PartialEq; re-serialization must reproduce the
        // exact byte stream.
        assert_eq!(serialize_state(&restored).unwrap(), bytes);
    }

    #[test]
    fn test_memory_memfd_roundtrip() {
        let mem = single_region_mem(0x10000);
        // Write a recognizable pattern into guest memory.
        let pattern: Vec<u8> = (0..0x1000u32).map(|i| (i % 251) as u8).collect();
        mem.write_slice(&pattern, GuestAddress(0x2000)).unwrap();

        let (file, size) = dump_memory_to_memfd(&mem).unwrap();
        assert_eq!(size, 0x10000);

        // Read the raw dump back and compare against guest memory.
        let mut dump = vec![0u8; size as usize];
        use std::os::unix::fs::FileExt;
        file.read_exact_at(&mut dump, 0).unwrap();

        let mut guest_bytes = vec![0u8; 0x1000];
        mem.read_slice(&mut guest_bytes, GuestAddress(0x2000))
            .unwrap();
        assert_eq!(&dump[0x2000..0x3000], &guest_bytes[..]);
        assert_eq!(&guest_bytes[..], &pattern[..]);
    }

    #[test]
    fn snapshot_export_is_an_independent_file_pair() {
        let state_file = vmm_sys_util::tempfile::TempFile::new().unwrap();
        let memory_file = vmm_sys_util::tempfile::TempFile::new().unwrap();
        let state = vec![1, 2, 3, 4];
        let mut memory = MemfdOptions::default()
            .create("theseus-export-test")
            .unwrap()
            .into_file();
        memory.set_len(5).unwrap();
        memory.write_all(b"hello").unwrap();
        let branch = BranchPoint {
            state_bytes: state.clone(),
            memory,
            mem_size: 5,
            base_seed: 1,
            branch_count: 0,
        };

        branch
            .export_snapshot(state_file.as_path(), memory_file.as_path())
            .unwrap();

        assert_eq!(std::fs::read(state_file.as_path()).unwrap(), state);
        assert_eq!(std::fs::read(memory_file.as_path()).unwrap(), b"hello");
    }

    #[test]
    fn test_child_seeds_are_deterministic_and_distinct() {
        // splitmix64 from the same inputs must be reproducible...
        let seeds_a: Vec<u64> = (0..4).map(|i| splitmix64(7 ^ (i << 32))).collect();
        let seeds_b: Vec<u64> = (0..4).map(|i| splitmix64(7 ^ (i << 32))).collect();
        assert_eq!(seeds_a, seeds_b);

        // ...and distinct across branch indices and base seeds.
        let unique: std::collections::HashSet<u64> = seeds_a.iter().copied().collect();
        assert_eq!(unique.len(), 4);
        assert_ne!(splitmix64(7), splitmix64(8));
    }

    #[test]
    fn test_targeted_child_seed_does_not_consume_a_branch_slot() {
        let mut branch = BranchPoint {
            state_bytes: Vec::new(),
            memory: File::open("/dev/null").unwrap(),
            mem_size: 0,
            base_seed: 7,
            branch_count: 0,
        };
        let first = branch.child_seed_at(0);
        assert_eq!(branch.child_seed(), first);
        assert_eq!(branch.branch_count(), 1);
        assert_eq!(branch.child_seed_at(1), branch.child_seed());
    }
}
