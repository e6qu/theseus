// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Theseus guest SDK — the shared contract between the host-side control
//! device (`vmm::devices::pseudo::theseus`) and guest code.
//!
//! `no_std`: usable from bare-metal guests, guest kernels, and (via a driver)
//! Linux userspace. The host side reuses the constants; the
//! [`ControlChannel`] guest-side driver pokes the MMIO registers.

#![no_std]

#[cfg(feature = "std")]
extern crate std;

/// Device bus primitives (BusDevice/Bus), shared by the host VMM and the
/// Theseus engine. Moved out of `vmm::vstate` to keep the engine crate free
/// of a vmm dependency (no dependency cycles). Requires `std` (enabled by
/// the VMM; bare-metal guests use default-features = false).
#[cfg(feature = "std")]
pub mod bus;

/// Magic value at the magic register: ASCII "THES" (little-endian).
pub const MAGIC: u32 = u32::from_le_bytes(*b"THES");

/// Register offsets from the device's MMIO base.
pub const OFS_MAGIC: u64 = 0;
/// Status register (read).
pub const OFS_STATUS: u64 = 4;
/// Event register (read): pops one byte from the host→guest FIFO.
pub const OFS_EVENT: u64 = 5;
/// Command register (write).
pub const OFS_COMMAND: u64 = 6;
/// Log marker register (write).
pub const OFS_LOG: u64 = 7;

/// Status bit: the host→guest event FIFO is non-empty.
pub const STATUS_EVENTS_PENDING: u8 = 0x01;

/// Guest command: the workload finished setup and is ready for events.
pub const CMD_SETUP_COMPLETE: u8 = 0x01;

/// Event terminator: the host's current event round is over.
pub const EVENT_TERMINATOR: u8 = 0x00;

/// Marker: the guest booted and initialized.
pub const MARKER_BOOT: u8 = 0x42;
/// Marker: the guest finished processing the current event round.
pub const MARKER_DONE: u8 = 0xFF;

/// Guest-side driver for the control channel over MMIO.
pub struct ControlChannel {
    base: *mut u8,
}

impl ControlChannel {
    /// Create a channel for the device at `base`.
    ///
    /// # Safety
    ///
    /// `base` must be the MMIO address of a Theseus control device in this
    /// address space.
    pub unsafe fn new(base: *mut u8) -> Self {
        ControlChannel { base }
    }

    fn read(&self, offset: u64) -> u8 {
        // SAFETY: caller guarantees `base` is the device; offsets are in range.
        unsafe { self.base.add(offset as usize).read_volatile() }
    }

    fn write(&self, offset: u64, value: u8) {
        // SAFETY: as above.
        unsafe { self.base.add(offset as usize).write_volatile(value) }
    }

    /// True if this is a Theseus control device.
    pub fn detect(&self) -> bool {
        let mut magic = [0u8; 4];
        for (i, b) in magic.iter_mut().enumerate() {
            *b = self.read(OFS_MAGIC + i as u64);
        }
        u32::from_le_bytes(magic) == MAGIC
    }

    /// True when host→guest events are pending.
    pub fn events_pending(&self) -> bool {
        self.read(OFS_STATUS) & STATUS_EVENTS_PENDING != 0
    }

    /// Pop one event byte (0 when the FIFO is empty — poll
    /// [`Self::events_pending`] first).
    pub fn pop_event(&self) -> u8 {
        self.read(OFS_EVENT)
    }

    /// Issue a command (e.g. [`CMD_SETUP_COMPLETE`]).
    pub fn command(&self, cmd: u8) {
        self.write(OFS_COMMAND, cmd);
    }

    /// Emit an application-level marker byte.
    pub fn marker(&self, byte: u8) {
        self.write(OFS_LOG, byte);
    }

    /// Wait (spin) for host events.
    pub fn wait_events(&self) {
        while !self.events_pending() {}
    }

    /// The standard event round: echo every event back as a marker until the
    /// terminator, then emit [`MARKER_DONE`].
    pub fn event_round(&self) {
        loop {
            self.wait_events();
            let event = self.pop_event();
            if event == EVENT_TERMINATOR {
                break;
            }
            self.marker(event);
        }
        self.marker(MARKER_DONE);
    }
}

/// Linux-guest transport: the control channel over the serial console.
///
/// Markers are structured lines written to `/dev/ttyS0` (`THES:M:xx`);
/// events are lines read from it (`THES:E:xx`). Needs no guest driver —
/// the 8250 UART is in every stock kernel — at the cost of sharing the
/// console with kernel logs (the line protocol is unambiguous).
#[cfg(feature = "std")]
pub mod linux {
    use std::fs::{File, OpenOptions};
    use std::io::{self, BufRead, BufReader, Write};
    use std::string::String;

    /// Marker line prefix (guest→host).
    pub const MARKER_PREFIX: &str = "THES:M:";
    /// Event line prefix (host→guest).
    pub const EVENT_PREFIX: &str = "THES:E:";

    /// The serial-console control channel.
    pub struct TtyChannel {
        out: File,
        input: BufReader<File>,
    }

    impl TtyChannel {
        /// Open the console UART for the channel.
        pub fn console() -> io::Result<Self> {
            let out = OpenOptions::new().write(true).open("/dev/ttyS0")?;
            let input = OpenOptions::new().read(true).open("/dev/ttyS0")?;
            Ok(TtyChannel {
                out,
                input: BufReader::new(input),
            })
        }

        /// Emit a marker byte (host sees a `THES:M:xx` line).
        pub fn marker(&mut self, byte: u8) -> io::Result<()> {
            writeln!(self.out, "{MARKER_PREFIX}{byte:02x}")
        }

        /// Read the next event byte (blocking; skips non-channel lines such
        /// as kernel logs).
        pub fn next_event(&mut self) -> io::Result<u8> {
            loop {
                let mut line = String::new();
                if self.input.read_line(&mut line)? == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "console closed"));
                }
                if let Some(rest) = line.trim().strip_prefix(EVENT_PREFIX) {
                    if let Ok(byte) = u8::from_str_radix(rest, 16) {
                        return Ok(byte);
                    }
                }
            }
        }

        /// The standard event round: echo events as markers until the
        /// terminator, then emit MARKER_DONE.
        pub fn event_round(&mut self) -> io::Result<()> {
            loop {
                let event = self.next_event()?;
                if event == crate::EVENT_TERMINATOR {
                    break;
                }
                self.marker(event)?;
            }
            self.marker(crate::MARKER_DONE)
        }
    }
}
