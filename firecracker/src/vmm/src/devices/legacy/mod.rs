// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Implements legacy devices (UART, RTC etc).
mod i8042;
#[cfg(target_arch = "aarch64")]
pub mod rtc_pl031;
pub mod serial;

use std::io;
use std::ops::Deref;
use std::sync::{Arc, OnceLock, Weak};

use serde::Serializer;
use serde::ser::SerializeMap;
use vm_superio::Trigger;
use vmm_sys_util::eventfd::EventFd;

use crate::vstate::vcpu::MachineExecutionController;

pub use self::i8042::{I8042Device, I8042Error as I8042DeviceError};
#[cfg(target_arch = "aarch64")]
pub use self::rtc_pl031::RTCDevice;
pub use self::serial::{SerialDevice, SerialEventsWrapper, SerialWrapper};

/// Wrapper for implementing the trigger functionality for `EventFd`.
///
/// The trigger is used for handling events in the legacy devices.
#[derive(Debug)]
pub struct EventFdTrigger {
    event: EventFd,
    deferred: OnceLock<DeferredInterrupt>,
}

#[derive(Clone, Debug)]
struct DeferredInterrupt {
    controller: Weak<MachineExecutionController>,
    source: &'static str,
    gsi: u32,
}

impl Trigger for EventFdTrigger {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        if let Some(deferred) = self.deferred.get() {
            let controller = deferred.controller.upgrade().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "interrupt controller was dropped")
            })?;
            controller.request_interrupt(deferred.source, deferred.gsi);
            Ok(())
        } else {
            self.write(1)
        }
    }
}

impl Deref for EventFdTrigger {
    type Target = EventFd;
    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

impl EventFdTrigger {
    /// Clone an `EventFdTrigger`.
    pub fn try_clone(&self) -> io::Result<Self> {
        let cloned = Self::new((**self).try_clone()?);
        if let Some(deferred) = self.deferred.get() {
            cloned
                .deferred
                .set(deferred.clone())
                .expect("new interrupt trigger is unconfigured");
        }
        Ok(cloned)
    }

    /// Create an `EventFdTrigger`.
    pub fn new(evt: EventFd) -> Self {
        Self {
            event: evt,
            deferred: OnceLock::new(),
        }
    }

    /// Route future triggers through the deterministic machine scheduler.
    pub(crate) fn defer_interrupt(
        &self,
        controller: Arc<MachineExecutionController>,
        source: &'static str,
        gsi: u32,
    ) -> io::Result<()> {
        self.deferred
            .set(DeferredInterrupt {
                controller: Arc::downgrade(&controller),
                source,
                gsi,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::AlreadyExists, "interrupt already routed"))
    }

    /// Get the associated event fd out of an `EventFdTrigger`.
    pub fn get_event(&self) -> EventFd {
        self.event.try_clone().unwrap()
    }
}

/// Called by METRICS.flush(), this function facilitates serialization of aggregated metrics.
pub fn flush_metrics<S: Serializer>(serializer: S) -> Result<S::Ok, S::Error> {
    let mut seq = serializer.serialize_map(Some(1))?;
    seq.serialize_entry("i8042", &i8042::METRICS)?;
    #[cfg(target_arch = "aarch64")]
    seq.serialize_entry("rtc", &rtc_pl031::METRICS)?;
    seq.serialize_entry("uart", &serial::METRICS)?;
    seq.end()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn execution_ledger_leaves_ordinary_interrupts_on_the_eventfd() {
        let trigger = EventFdTrigger::new(EventFd::new(libc::EFD_NONBLOCK).unwrap());

        trigger.trigger().unwrap();

        assert_eq!(trigger.read().unwrap(), 1);
    }

    #[test]
    fn execution_ledger_queues_deferred_interrupts_from_trigger_clones() {
        let controller = Arc::new(MachineExecutionController::default());
        let trigger = EventFdTrigger::new(EventFd::new(libc::EFD_NONBLOCK).unwrap());
        trigger
            .defer_interrupt(Arc::clone(&controller), "serial", 4)
            .unwrap();
        let cloned = trigger.try_clone().unwrap();

        trigger.trigger().unwrap();
        cloned.trigger().unwrap();

        assert_eq!(
            controller.pending_interrupts_for_test(),
            [("serial", 4)]
        );
    }
}
