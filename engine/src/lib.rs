// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Theseus engine — leaf deterministic components, kept outside the
//! `firecracker/` tree (license boundary: this crate is AGPL-3.0-or-later;
//! the fork is Apache-2.0). Depends only on `theseus-sdk`, never on `vmm`
//! (dependency cycles are not allowed); the `vmm` crate depends on this
//! crate and re-exports these modules so fork-internal paths keep working.

pub mod detrng;
pub mod door;
pub mod simnet;
pub mod vclock;
