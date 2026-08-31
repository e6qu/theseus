// Copyright 2026 Theseus contributors.
// SPDX-License-Identifier: Apache-2.0

//! Theseus control channel — the guest↔host door (our `VMCALL` equivalent).
//!
//! A minimal MMIO device through which the deterministic environment
//! communicates with the guest:
//! - host → guest: a FIFO of event bytes (commands from the orchestrator)
//! - guest → host: lifecycle commands and application-level log markers
//!
//! Every byte the guest consumes from the event FIFO, and every command the
//! guest issues, is a potential **branch point** in the timeline tree. Commands
//! and log markers are recorded in `event_log`; event consumption order is
//! implicit in the FIFO.
//!
//! The guest talks to the device with raw loads/stores at a fixed MMIO
//! address (`layout::THESEUS_MEM_START`), so no ACPI/FDT entry or kernel
//! driver is required. (Simplified during implementation: MMIO on both
//! architectures — one code path, following the boot-timer precedent.)

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Barrier;

pub use theseus_sdk::{CMD_SETUP_COMPLETE, MAGIC};
use theseus_sdk::{
    OFS_COMMAND, OFS_EVENT, OFS_LOG, OFS_MAGIC, OFS_STATUS, STATUS_EVENTS_PENDING,
};

use crate::vstate::bus::BusDevice;

/// Size of the MMIO register range.
pub const THESEUS_MEM_LEN: u64 = 0x8;

/// A guest→host event, recorded in order of arrival.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// Guest signalled setup completion (`CMD_SETUP_COMPLETE`).
    SetupComplete,
    /// Guest issued an unrecognized command byte.
    Command(u8),
    /// Guest emitted an application-level log marker.
    GuestLog(u8),
}

/// The Theseus control device. Always present on the PIO bus in this fork.
#[derive(Debug, Default)]
pub struct TheseusDevice {
    /// Host→guest event FIFO. The orchestrator enqueues bytes; the guest pops
    /// them one at a time via [`OFS_EVENT`].
    host_events: VecDeque<u8>,
    /// Ordered guest→host event log (commands + log markers).
    event_log: Vec<ControlEvent>,
}

impl TheseusDevice {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue an event byte for the guest to consume.
    pub fn push_event(&mut self, byte: u8) {
        self.host_events.push_back(byte);
    }

    /// The recorded guest→host events, in arrival order.
    pub fn event_log(&self) -> &[ControlEvent] {
        &self.event_log
    }

    /// Take the recorded events, leaving the log empty.
    pub fn drain_event_log(&mut self) -> Vec<ControlEvent> {
        std::mem::take(&mut self.event_log)
    }
}

impl BusDevice for TheseusDevice {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        match offset {
            ofs if ofs < 4 => {
                // Magic window: byte at register offset `ofs` is magic[ofs].
                let magic = MAGIC.to_le_bytes();
                for (i, b) in data.iter_mut().enumerate() {
                    let idx = ofs as usize + i;
                    if idx < 4 {
                        *b = magic[idx];
                    }
                }
            }
            OFS_STATUS if data.len() == 1 => {
                data[0] = if self.host_events.is_empty() {
                    0
                } else {
                    STATUS_EVENTS_PENDING
                };
            }
            OFS_EVENT if data.len() == 1 => {
                data[0] = self.host_events.pop_front().unwrap_or(0);
            }
            _ => {}
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        if data.len() != 1 {
            return None;
        }
        match offset {
            OFS_COMMAND => {
                let event = if data[0] == CMD_SETUP_COMPLETE {
                    ControlEvent::SetupComplete
                } else {
                    ControlEvent::Command(data[0])
                };
                self.event_log.push(event);
            }
            OFS_LOG => {
                self.event_log.push(ControlEvent::GuestLog(data[0]));
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_magic() {
        let mut dev = TheseusDevice::new();
        let mut data = [0u8; 4];
        dev.read(0, OFS_MAGIC, &mut data);
        assert_eq!(&data, b"THES");

        // Per-byte reads (as a guest without wide MMIO loads would do) must
        // return the corresponding magic bytes.
        for (i, expected) in b"THES".iter().enumerate() {
            let mut byte = [0u8];
            dev.read(0, i as u64, &mut byte);
            assert_eq!(byte[0], *expected);
        }
    }

    #[test]
    fn test_event_fifo_roundtrip() {
        let mut dev = TheseusDevice::new();
        let mut byte = [0xFFu8];

        // Empty FIFO: status clear, reads return 0.
        dev.read(0, OFS_STATUS, &mut byte);
        assert_eq!(byte[0], 0);
        dev.read(0, OFS_EVENT, &mut byte);
        assert_eq!(byte[0], 0);

        // Host pushes events; guest observes pending status and FIFO order.
        dev.push_event(0xAA);
        dev.push_event(0xBB);
        dev.read(0, OFS_STATUS, &mut byte);
        assert_eq!(byte[0] & STATUS_EVENTS_PENDING, STATUS_EVENTS_PENDING);
        dev.read(0, OFS_EVENT, &mut byte);
        assert_eq!(byte[0], 0xAA);
        dev.read(0, OFS_EVENT, &mut byte);
        assert_eq!(byte[0], 0xBB);
        dev.read(0, OFS_STATUS, &mut byte);
        assert_eq!(byte[0], 0);
    }

    #[test]
    fn test_command_and_log_recording() {
        let mut dev = TheseusDevice::new();

        dev.write(0, OFS_COMMAND, &[CMD_SETUP_COMPLETE]);
        dev.write(0, OFS_COMMAND, &[0x42]);
        dev.write(0, OFS_LOG, &[0x01]);

        assert_eq!(
            dev.event_log(),
            &[
                ControlEvent::SetupComplete,
                ControlEvent::Command(0x42),
                ControlEvent::GuestLog(0x01),
            ]
        );

        let drained = dev.drain_event_log();
        assert_eq!(drained.len(), 3);
        assert!(dev.event_log().is_empty());
    }

    #[test]
    fn test_stray_accesses_have_no_side_effects() {
        let mut dev = TheseusDevice::new();

        // Wide writes are ignored.
        dev.write(0, OFS_COMMAND, &[CMD_SETUP_COMPLETE, 0x00]);
        assert!(dev.event_log().is_empty());

        // Unknown offsets (beyond the register window) are inert.
        let mut data = [0xABu8; 2];
        dev.read(0, 0x10, &mut data);
        assert_eq!(data, [0xAB, 0xAB]);
        dev.write(0, 0x10, &[0x01]);
        assert!(dev.event_log().is_empty());

        // Writes to the read-only magic window are inert too.
        dev.write(0, OFS_MAGIC, &[0x01]);
        assert!(dev.event_log().is_empty());
    }
}
