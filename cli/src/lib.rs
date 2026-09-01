// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The stable boundary between a Theseus test directory and the runner.
//!
//! P6.1 deliberately stops at validation and planning. The returned run plan
//! is what the executor and replay bundle will consume in P6.2.

mod manifest;

pub use manifest::{load_plan, LoadError, RunPlan};
