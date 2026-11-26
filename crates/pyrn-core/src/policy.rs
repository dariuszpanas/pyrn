//! Pure in-memory zero-checkout test planning API for library embedders and CLI consumers.
//!
//! - Evaluates directly over `base_tree: &dyn TreeSource` and `candidate_tree: &dyn TreeSource`
//!   (e.g. virtual snapshots or content-addressed Merkle trees) with ZERO checkout and ZERO subprocesses.
//! - Reads authoritative project policy (`.pyrn.toml` / `pyproject.toml`) from `base_tree`
//!   (fallback `candidate_tree` only if empty-repo seed), preventing candidate branches from
//!   weakening their own test selection policy.
//! - Computes exact AST & symbol-level diffs directly between `base_tree` and `candidate_tree`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::config::PyrnConfig;
use crate::dynamic::DynamicTraceIndex;
use crate::graph::{ChangedFileSpec, DependencyGraph, GraphConfig, ImpactResult};
use crate::key::TestCacheKeyRecord;
use crate::parser::{diff_python_symbols, parse_python_file};
use crate::receipt::{CompactReceipt, ConfidenceTier, ExecVerdict, RunMetrics, SlotRole};
use crate::tree::{DOMAIN_RAW_BLOB, TreeSource, blake3_domain_hex};

/// Input options for zero-checkout in-memory test planning (`plan_python_tests`).
#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub tier: ConfidenceTier,
    pub role: SlotRole,
    pub platform_tag: String,
    pub python_version: String,
    pub django_settings_module: String,
    pub env_values: BTreeMap<String, String>,
    /// Optional explicit changed paths; when `None`, paths are computed by diffing `base_tree` vs `candidate_tree`.
    pub explicit_changed: Option<Vec<ChangedFileSpec>>,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            tier: ConfidenceTier::StrictImpacted,
            role: SlotRole::Agent,
            platform_tag: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            python_version: "3.12".to_string(),
            django_settings_module: "default".to_string(),
            env_values: BTreeMap::new(),
            explicit_changed: None,
        }
    }
}

/// Complete in-memory test plan returned by `plan_python_tests`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestPlan {
    pub env_fingerprint: String,
    pub project_fingerprint: String,
    pub impact: ImpactResult,
    pub selected_files: Vec<String>,
    pub selected_nodeids: Vec<String>,
    pub cache_keys: Vec<TestCacheKeyRecord>,
    pub metrics: RunMetrics,
    pub receipt: CompactReceipt,
    pub parse_and_hash_ms: f64,
    pub graph_build_ms: f64,
}

/// Compute exact changed files AND symbol-level Python AST diffs directly between two `TreeSource`s
/// with zero filesystem checkout and zero `git` subprocesses!
#[must_use]
pub fn diff_trees_with_symbols(
    base_tree: &dyn TreeSource,
    candidate_tree: &dyn TreeSource,
    config: &PyrnConfig,
) -> Vec<ChangedFileSpec> {
    let mut all_paths: BTreeSet<&str> = BTreeSet::new();
    for p in base_tree.paths() {
        all_paths.insert(p.as_str());
    }
    for p in candidate_tree.paths() {
        all_paths.insert(p.as_str());
    }

    let mut specs = Vec::new();
    for path in all_paths {
        match (base_tree.get(path), candidate_tree.get(path)) {
            (Some(b), Some(c)) if b == c => continue,
            (Some(b), Some(c)) if path.ends_with(".py") => {
                let raw_b = blake3_domain_hex(DOMAIN_RAW_BLOB, b);
                let raw_c = blake3_domain_hex(DOMAIN_RAW_BLOB, c);
                let parsed_b = parse_python_file(path, b, &raw_b, config);
                let parsed_c = parse_python_file(path, c, &raw_c, config);
                let syms = diff_python_symbols(&parsed_b, &parsed_c);
                specs.push(ChangedFileSpec::with_symbols(path, syms));
            }
            // Added, deleted, or modified non-Python file -> conservative `All`
            _ => {
                specs.push(ChangedFileSpec::all(path));
            }
        }
    }
    specs
}

/// Pure in-memory zero-checkout test planning entrypoint for library embedders
/// and local CLI `pyrn plan`.
///
/// Policy authority rule:
/// Reads `.pyrn.toml` / `pyproject.toml` from `base_tree` when non-empty (falling back to
/// `candidate_tree` only on empty-repo seed), so candidate edits cannot weaken test selection.
#[must_use]
pub fn plan_python_tests(
    base_tree: Option<&dyn TreeSource>,
    candidate_tree: &dyn TreeSource,
    dynamic: Option<&DynamicTraceIndex>,
    opts: &PlanOptions,
) -> TestPlan {
    // Authoritative config from `base_tree` (fallback `candidate_tree` if base is absent/empty)
    let auth_project = match base_tree {
        Some(bt) if !bt.paths().is_empty() => PyrnConfig::from_tree(bt),
        _ => PyrnConfig::from_tree(candidate_tree),
    };

    let base_graph_cfg = GraphConfig {
        project: auth_project.clone(),
        conftest_policy: auth_project.conftest_policy,
        unknown_path_policy: auth_project.unknown_path_policy,
        symbol_level_tia: auth_project.symbol_level_tia,
        platform_tag: opts.platform_tag.clone(),
        python_version: opts.python_version.clone(),
        django_settings_module: opts.django_settings_module.clone(),
    };
    let (eff_cfg, _force_raw) = base_graph_cfg.effective_for_tier(opts.tier);

    let graph = DependencyGraph::build(candidate_tree, dynamic, eff_cfg);

    let changed_specs = match &opts.explicit_changed {
        Some(explicit) => explicit.clone(),
        None => match base_tree {
            Some(bt) => diff_trees_with_symbols(bt, candidate_tree, &auth_project),
            None => Vec::new(),
        },
    };

    // If neither explicit changes nor a base tree was given, select full suite conservatively
    let impact = if opts.explicit_changed.is_none() && base_tree.is_none() {
        graph.compute_impact_with_symbols(&[], ConfidenceTier::FullVerified)
    } else {
        graph.compute_impact_with_symbols(&changed_specs, opts.tier)
    };

    let selected_files: Vec<String> = impact
        .impacted
        .iter()
        .map(|i| i.test_file.clone())
        .collect();
    let selected_set: BTreeSet<&str> = selected_files.iter().map(String::as_str).collect();
    let selected_nodeids: Vec<String> = graph
        .tests
        .iter()
        .filter(|t| selected_set.contains(t.file_path.as_str()))
        .map(|t| t.nodeid.clone())
        .collect();

    let cache_keys = graph.compute_all_cache_keys(&opts.env_values, opts.tier);

    let metrics = RunMetrics {
        tests_total: impact.total_test_nodes as i64,
        tests_selected: impact.impacted_test_nodes as i64,
        tests_cached: (impact
            .total_test_nodes
            .saturating_sub(impact.impacted_test_nodes)) as i64,
        tests_tainted: impact.tainted_test_nodes as i64,
        tia_parse_ms: graph.timings.parse_and_hash_ms.round() as i64,
        tia_graph_ms: graph.timings.graph_build_ms.round() as i64,
        ..RunMetrics::default()
    };

    let summary = format!(
        "plan tier={} selected={}/{} files={}/{} reduction={:.1}% symbol_pruned={}",
        impact.effective_tier.as_str(),
        impact.impacted_test_nodes,
        impact.total_test_nodes,
        impact.impacted_test_files,
        impact.total_test_files,
        impact.reduction_pct,
        impact.symbol_pruned_edges,
    );

    let receipt = CompactReceipt {
        schema: CompactReceipt::SCHEMA_V1.to_string(),
        verdict: ExecVerdict::Pass,
        tier: impact.effective_tier,
        role: opts.role,
        env_fingerprint: graph.env_fingerprint[..16.min(graph.env_fingerprint.len())].to_string(),
        wall_ms: (graph.timings.total_ms + impact.eval_ms).round() as u64,
        tests_total: impact.total_test_nodes,
        tests_selected: impact.impacted_test_nodes,
        tests_cached: impact
            .total_test_nodes
            .saturating_sub(impact.impacted_test_nodes),
        tests_tainted: impact.tainted_test_nodes,
        tests_passed: 0,
        tests_failed: 0,
        tests_skipped: 0,
        tests_flaky: 0,
        worker_restarts: 0,
        fast_path_tests: 0,
        crash_capsule: None,
        summary,
    };

    TestPlan {
        env_fingerprint: graph.env_fingerprint,
        project_fingerprint: graph.project_fingerprint,
        impact,
        selected_files,
        selected_nodeids,
        cache_keys,
        metrics,
        receipt,
        parse_and_hash_ms: graph.timings.parse_and_hash_ms,
        graph_build_ms: graph.timings.graph_build_ms,
    }
}
