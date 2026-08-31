// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Theseus orchestrator — timeline branching, ground-truth coverage, and
//! the exploration engine. Depends on `vmm` one-way (no cycles): `vmm` does
//! not depend on this crate.

pub mod branch;
pub mod coverage;
pub mod orchestrator;
