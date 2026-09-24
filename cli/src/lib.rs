// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The stable boundary between a Theseus test directory and the runner.
//!
//! It validates test inputs, produces locked plans, executes supported KVM
//! workloads, and reads or compares retained evidence.

mod cargo_coverage;
mod compare;
mod compose;
mod evaluation;
mod evidence;
mod execution;
mod explore;
mod go_coverage;
mod history;
mod java_coverage;
mod manifest;
mod query;
mod report;
mod runner;
mod status;
mod topology_evidence;

pub use topology_evidence::{verify_topology_bundle, TopologyBundleSummary};

pub use cargo_coverage::{
    cargo_coverage, cargo_coverage_rustc_wrapper, is_cargo_coverage_wrapper, CargoCoverageOutput,
    CARGO_COVERAGE_USAGE,
};
pub use compare::{
    boundary_at_moment, compare_campaigns, compare_forked_campaigns, query_campaigns,
    BoundaryMomentDiff, CampaignComparison, CampaignQuery, CompareError, ForkedComparison,
};
pub use compose::{
    explore_compose, explore_compose_expect_counterexample,
    explore_compose_expect_counterexample_with, explore_compose_forked, explore_compose_with,
    load_compose_plan, minimize_compose_campaign, minimize_compose_campaign_expect_counterexample,
    replay_compose, test_compose, CampaignGuidance, ComposeError, ComposePlan,
};
pub use evaluation::{
    capture_evaluation, evaluate, write_evaluation_lock, EvaluationError, EvaluationSummary,
};
pub use evidence::{verify_native_evidence, EvidenceError, NativeEvidenceSummary};
pub use explore::{
    explore, minimize_exploration_path, replay_exploration, replay_exploration_path,
    snapshot_exploration_path, ExploreError,
};
pub use go_coverage::{go_coverage, GoCoverageOutput, GO_COVERAGE_USAGE};
pub use history::{
    property_history, CampaignPropertyHistory, HistoryError, PropertyHistoryEntry,
    PropertyVerdictRecord,
};
pub use java_coverage::{java_coverage, JavaCoverageOutput, JAVA_COVERAGE_USAGE};
pub use manifest::{
    load_plan, ArtifactPlan, CheckKind, CheckPlan, ExplorePlan, LoadError, Novelty,
    ReplayFingerprint, ReplayTreeNode, RunPlan,
};
pub use query::{
    collect_moment, find_moment, list_moments, next_moment, next_moment_in, previous_moment,
    previous_moment_in, query_moment, query_temporal, temporal_query, CollectedFile,
    CollectedMoment, MomentError, MomentHit, MomentSummary, NeedleOccurrence, TemporalQuery,
    TemporalRelation,
};
pub use report::{report, report_file, report_text, ReportError, ReportFormat};
pub use runner::{replay, replay_to, test, ReplayResult, RunError, TestResult};
pub use status::{
    campaign_status, ArtifactInventory, CampaignStatus, PropertyVerdict, StatusError,
};
