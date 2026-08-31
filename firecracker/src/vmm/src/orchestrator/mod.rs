// Copyright 2026 Theseus contributors.
// SPDX-License-Identifier: Apache-2.0

//! Theseus orchestrator — timeline tree and child spawning.
//!
//! This is where branch points become a multiverse: [`tree::TimelineTree`]
//! tracks the timelines and their deterministic exploration order, and
//! [`spawn::spawn_child`] forks a paused microVM into a new timeline that
//! diverges only by seed.
//!
//! Coverage-guided search builds on this in a later step; the tree's
//! deterministic order is the foundation it will prioritize.

pub mod explorer;
pub mod spawn;
pub mod tree;
