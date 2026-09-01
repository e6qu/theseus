// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The stable boundary between a Theseus test directory and the runner.
//!
//! P6.1 deliberately stops at validation and planning. The returned run plan
//! is what the executor and replay bundle will consume in P6.2.

mod compose;
mod explore;
mod manifest;
mod report;
mod runner;

pub use compose::{load_compose_plan, replay_compose, test_compose, ComposeError, ComposePlan};
pub use explore::{
    explore, minimize_exploration_path, replay_exploration, replay_exploration_path,
    snapshot_exploration_path, ExploreError,
};
pub use manifest::{
    load_plan, ArtifactPlan, CheckKind, CheckPlan, ExplorePlan, LoadError, Novelty, RunPlan,
};
pub use report::{report, ReportError};
pub use runner::{replay, test, ReplayResult, RunError, TestResult};
