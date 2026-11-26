#![forbid(unsafe_code)]

//! `pyrn-runner` — Isolated warm-pool Rust execution engine, container-aware worker resource
//! budgeter, and unified TIA + CAS-cached pytest pipeline.
//!
//! Public entry points for `pyrn-cli` and programmatic callers:
//! - [`execute_unified_run`]: Unified pipeline supporting full execution, `--tia` symbol-level
//!   impact selection, `--cache` SQLite WAL CAS hits/seeding, `--daemon` persistent warm pool,
//!   `--trace` PEP 669 + PEP 578 + `os.environ` dynamic tracing, and `--cov-sysmon` coverage.
//! - [`execute_cold_run`] / [`execute_via_daemon`]: Direct cold or warm-daemon execution.
//! - [`serve_daemon`] / [`daemon_status`] / [`daemon_stop`]: Persistent warm worker daemon control.
//! - [`detect_worker_budget`] / [`physical_default_workers`]: Container-aware physical-core sizing.
//! - [`collect_doctor_report`]: Environment, cgroup budget, Python/sysmon, and daemon health checks.
//! - [`build_compact_receipt`] / [`format_metrics_kv`] / [`emit_metrics_kv`]: structured receipts and KV metrics.

pub mod daemon;
pub mod diagnostics;
pub mod discovery;
pub mod isolation;
pub mod protocol;
pub mod resources;
pub mod scheduler;
pub mod supervisor;

pub use daemon::{
    DaemonServeConfig, DaemonStateFile, daemon_status, daemon_stop, execute_via_daemon,
    execute_via_daemon_with_exclusions, send_daemon_request, serve_daemon,
};
pub use diagnostics::{
    DoctorReport, build_compact_receipt, build_run_metrics, collect_doctor_report, emit_metrics_kv,
    emit_ndjson_from_report, format_metrics_kv, print_human_summary,
};
pub use discovery::{
    DiscoveredFile, FileContentSig, RepoFingerprint, compute_migrations_hash,
    diff_changed_test_files, discover_test_files, scan_repo_fingerprint, to_rel_posix,
};
pub use isolation::{
    CacheLayout, EMBEDDED_TRACE_PLUGIN_PY, EMBEDDED_WORKER_PY, WorkerSandbox, clean_canonical_path,
    generate_auth_token, merge_coverage_shards, short_sandbox_root,
};
pub use protocol::{
    CompactReceipt, ConfidenceTier, CrashCapsule, DEFAULT_METRICS_PREFIX, DaemonRequest,
    DaemonResponse, DynamicTestTrace, DynamicTraceFile, ExecVerdict, MAX_CRASH_CAPSULE_BYTES,
    MAX_RECEIPT_BYTES, ResourceLane, RunMetrics, RunReport, SlotRole, StreamEvent, TestOutcome,
    TestRecord, WorkerCommand, WorkerEvent, WorkerStats,
};
pub use resources::{
    DEFAULT_WORKER_MEM_MB, EffectiveWorkerBudget, HostConstraints, ResourceBudget,
    RunnerResourceConfig, detect_host_constraints, detect_worker_budget,
    load_runner_resource_config, parse_cgroup_memory_max, parse_cgroup_v1_quota,
    parse_cgroup_v2_cpu_max, parse_cpuset_list, parse_proc_self_cgroup_v2_rel,
    physical_default_workers, resolve_worker_budget,
};
pub use scheduler::{AffinityWorkQueue, TimingHistory, WorkUnit, plan_worker_split};
pub use supervisor::{
    LiveWorker, RunConfig, WorkerSpawnSpec, compute_verdict, emit_stream, empty_report,
    execute_cold_run, execute_cold_run_with_exclusions, execute_on_pool, find_conftest_seed_dir,
    spawn_worker, spawn_worker_pool,
};

use anyhow::{Context, Result};
use pyrn_core::cache::{CachedTestEntry, CasTestCache};
use pyrn_core::config::PyrnConfig;
use pyrn_core::dynamic::{DynamicTraceIndex, base_nodeid};
use pyrn_core::git_replay::git_changed_specs_against_base;
use pyrn_core::graph::{ChangedFileSpec, DependencyGraph, GraphConfig, ImpactResult};
use pyrn_core::key::TestCacheKeyRecord;
use pyrn_core::tree::{InMemoryTree, TreeSource, normalize_rel_path};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// High-level options for [`execute_unified_run`] combining execution, TIA, CAS cache, and daemon.
#[derive(Clone, Debug, Default)]
pub struct UnifiedRunOptions {
    pub run: RunConfig,
    /// Route execution misses through an already-running warm persistent daemon.
    pub daemon: bool,
    /// Enable symbol-level Test Impact Analysis via `pyrn-core`.
    pub tia: bool,
    /// Enable SQLite WAL content-addressed test result caching via `pyrn-core::CasTestCache`.
    pub cache: bool,
    /// Git base ref for TIA symbol diffing (e.g. `"HEAD"`, `"main"`, `"origin/main"`).
    pub base_ref: Option<String>,
    /// Explicit changed file paths for TIA (takes precedence over `base_ref` when non-empty).
    pub changed_files: Vec<String>,
    /// Optional pre-existing dynamic trace JSON path to refine TIA edges & cache keys.
    pub trace_in: Option<PathBuf>,
    /// Optional output path to write collected `DynamicTraceFile` JSON.
    pub trace_out: Option<PathBuf>,
    /// Optional commit SHA metadata recorded when seeding CAS cache entries.
    pub commit_sha: Option<String>,
}

/// Complete outcome of [`execute_unified_run`], ready for CLI output, receipts, and KV metrics.
#[derive(Clone, Debug)]
pub struct UnifiedRunOutcome {
    pub report: RunReport,
    pub impact: Option<ImpactResult>,
    pub env_fingerprint: String,
    pub cache_seeded: usize,
    pub resource_budget: ResourceBudget,
}

/// Execute the unified `pyrn` pipeline:
/// 1. When neither `--tia` nor `--cache` is enabled, runs directly in cold or `--daemon` pool.
/// 2. When `--tia` and/or `--cache` is enabled:
///    - Loads `InMemoryTree` + `PyrnConfig` + optional `DynamicTraceIndex` and builds `DependencyGraph`.
///    - Computes impacted test files (for `--tia`) and BLAKE3 closure cache keys (for `--cache`).
///    - Serves verified passing (`outcome == "passed"`), non-tainted cache hits from `CasTestCache`
///      (respecting `SlotRole::{Agent, Verify}` segregation).
///    - Executes ONLY uncached impacted tests in the cold or `--daemon` warm pool (collecting PEP 669
///      + PEP 578 + `os.environ` traces when caching is active so dynamic taints/env-reads are enforced).
///    - Seeds ONLY all-passing, non-tainted test families back into `CasTestCache`.
pub async fn execute_unified_run(opts: &UnifiedRunOptions) -> Result<UnifiedRunOutcome> {
    execute_unified_run_with_env(opts, None).await
}

/// Like [`execute_unified_run`], allowing callers/tests to inject an explicit environment map
/// (e.g. `DJANGO_SETTINGS_MODULE`) without mutating process-global environment variables.
pub async fn execute_unified_run_with_env(
    opts: &UnifiedRunOptions,
    env_override: Option<&BTreeMap<String, String>>,
) -> Result<UnifiedRunOutcome> {
    let t_total = Instant::now();
    let project_root =
        clean_canonical_path(fs::canonicalize(&opts.run.project_root).with_context(|| {
            format!("resolve project_root {}", opts.run.project_root.display())
        })?);

    let mut run_cfg = opts.run.clone();
    run_cfg.project_root = project_root.clone();

    let migrations_hash = compute_migrations_hash(&project_root, &run_cfg.python_exe);
    let layout = CacheLayout::prepare(
        run_cfg.cache_dir.as_deref(),
        run_cfg.role,
        &project_root,
        &run_cfg.python_exe,
        &migrations_hash,
    )?;
    let resource_budget = detect_worker_budget(Some(&project_root), run_cfg.workers, None);

    // Fast direct path when neither TIA nor CAS caching is enabled.
    if !opts.tia && !opts.cache {
        let report = if opts.daemon {
            let rep = execute_via_daemon(&run_cfg).await?;
            if run_cfg.ndjson {
                emit_ndjson_from_report(&rep, &project_root);
            }
            rep
        } else {
            execute_cold_run(&run_cfg).await?
        };

        if let Some(ref trace_out) = opts.trace_out
            && !report.traces.is_empty()
        {
            write_trace_file(trace_out, &report.traces, report.wall_ms as f64)?;
        }

        return Ok(UnifiedRunOutcome {
            report,
            impact: None,
            env_fingerprint: layout.env_fingerprint,
            cache_seeded: 0,
            resource_budget,
        });
    }

    // --- Unified TIA + CAS Cache Pipeline ---
    let pyrn_cfg = PyrnConfig::load_from_dir(&project_root);
    let tree = InMemoryTree::load_from_fs(&project_root)
        .with_context(|| format!("load repo tree {}", project_root.display()))?;

    let persisted_trace_path = layout.project_dir.join("dynamic_traces.json");
    let mut dynamic_idx = DynamicTraceIndex::default();
    if let Some(ref trace_in) = opts.trace_in
        && trace_in.is_file()
        && let Ok(loaded) = DynamicTraceIndex::load_json(trace_in)
    {
        for tr in loaded.by_nodeid.into_values() {
            dynamic_idx.merge(DynamicTraceFile {
                schema_version: 1,
                python_version: String::new(),
                monitoring_backend: "loaded".to_owned(),
                total_wall_ms: 0.0,
                tests: vec![tr],
            });
        }
    } else if persisted_trace_path.is_file()
        && let Ok(loaded) = DynamicTraceIndex::load_json(&persisted_trace_path)
    {
        dynamic_idx = loaded;
    }

    let env_map = match env_override {
        Some(custom) => normalize_env_map(custom),
        None => collect_normalized_env(),
    };

    let mut base_graph_cfg = GraphConfig::from_project(pyrn_cfg.clone());
    if let Some(dsm) = env_map
        .get("DJANGO_SETTINGS_MODULE")
        .filter(|s| !s.is_empty())
    {
        base_graph_cfg.django_settings_module = dsm.clone();
    }
    let (eff_cfg, _) = base_graph_cfg.clone().effective_for_tier(run_cfg.tier);

    let graph = DependencyGraph::build(
        &tree,
        (!dynamic_idx.is_empty()).then_some(&dynamic_idx),
        eff_cfg,
    );
    let tia_parse_ms = graph.timings.parse_and_hash_ms.round() as u64;
    let tia_graph_ms = graph.timings.graph_build_ms.round() as u64;

    // 1. Determine selected files (either via TIA impact analysis or full discovery).
    // Include ALL discovered test files when `!opts.tia` so files with 0 static `def test_*`
    // functions (e.g. inherited `unittest.TestCase` subclasses or `pytest_generate_tests`) are NEVER dropped!
    let (impact_opt, selected_files): (Option<ImpactResult>, BTreeSet<String>) = if opts.tia {
        let changed_specs: Vec<ChangedFileSpec> = if !opts.changed_files.is_empty() {
            opts.changed_files
                .iter()
                .map(|p| ChangedFileSpec::all(p.as_str()))
                .collect()
        } else {
            let base = opts.base_ref.as_deref().unwrap_or("HEAD");
            git_changed_specs_against_base(&project_root, base, &pyrn_cfg).unwrap_or_default()
        };
        let imp = graph.compute_impact_with_symbols(&changed_specs, run_cfg.tier);
        let mut files: BTreeSet<String> = imp
            .impacted
            .iter()
            .map(|i| normalize_rel_path(&i.test_file))
            .collect();
        // Safety net for test files with 0 static `def test_*` functions (e.g. inherited
        // `unittest.TestCase` subclasses or `pytest_generate_tests` modules) that may not have
        // static entries in `graph.tests_by_file`: include them whenever `FullVerified` triggers
        // or they are reachable in `graph.reverse_deps` from any changed file!
        if let Ok(discovered) = discover_test_files(
            &project_root,
            &run_cfg.targets,
            run_cfg.markers.as_deref(),
            &pyrn_cfg,
        ) {
            let zero_static: Vec<String> = discovered
                .into_iter()
                .map(|df| normalize_rel_path(&df.rel_path))
                .filter(|rel| !graph.tests_by_file.contains_key(rel))
                .collect();
            if !zero_static.is_empty() {
                if imp.effective_tier == ConfidenceTier::FullVerified {
                    files.extend(zero_static);
                } else {
                    let reachable = reverse_reachable_from_changes(&graph, &changed_specs);
                    for rel in zero_static {
                        if reachable.contains(&rel) {
                            files.insert(rel);
                        }
                    }
                }
            }
        }
        if !run_cfg.targets.is_empty() {
            files.retain(|f| matches_targets(&project_root, f, &run_cfg.targets));
        }
        (Some(imp), files)
    } else {
        let mut files: BTreeSet<String> = graph.tests_by_file.keys().cloned().collect();
        if let Ok(discovered) = discover_test_files(
            &project_root,
            &run_cfg.targets,
            run_cfg.markers.as_deref(),
            &pyrn_cfg,
        ) {
            for df in discovered {
                files.insert(normalize_rel_path(&df.rel_path));
            }
        }
        if !run_cfg.targets.is_empty() {
            files.retain(|f| matches_targets(&project_root, f, &run_cfg.targets));
        }
        (None, files)
    };

    // 2. Compute BLAKE3 cache keys across selected test nodes.
    let effective_tier = impact_opt
        .as_ref()
        .map_or(run_cfg.tier, |i| i.effective_tier);
    let all_key_records = graph.compute_all_cache_keys(&env_map, effective_tier);

    let selected_key_records: Vec<TestCacheKeyRecord> = all_key_records
        .into_iter()
        .filter(|r| selected_files.contains(&r.file_path))
        .collect();

    let tests_selected = selected_key_records.len();
    let tests_tainted = selected_key_records
        .iter()
        .filter(|r| r.safety.is_tainted())
        .count();

    // Include marker/keyword expression in CAS env fingerprint when specified so filtered runs
    // and full runs never collide in the cache table.
    let cas_env_fp = compute_cas_env_fingerprint(
        &graph.env_fingerprint,
        run_cfg.markers.as_deref(),
        run_cfg.keyword.as_deref(),
    );

    // 3. Look up verified passing (`outcome == "passed"`) non-tainted cache hits and build a
    //    STRICT NEGATIVE SKIP-LIST (`exclude_by_file`) rather than a positive allowlist.
    //    This guarantees inherited `unittest.TestCase` methods and `pytest_generate_tests`
    //    items are NEVER silently dropped!
    let mut cached_records: Vec<TestRecord> = Vec::new();
    let mut cached_base_nodeids: BTreeSet<String> = BTreeSet::new();
    let mut exclude_by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut static_total_by_file: BTreeMap<String, usize> = BTreeMap::new();
    let mut static_cached_by_file: BTreeMap<String, usize> = BTreeMap::new();
    let mut key_by_base_nodeid: BTreeMap<String, TestCacheKeyRecord> = BTreeMap::new();

    let mut cas_opt = if opts.cache {
        Some(CasTestCache::open(&layout.cas_db_path)?)
    } else {
        None
    };

    if let Some(ref mut cas) = cas_opt {
        let cacheable_keys: Vec<String> = selected_key_records
            .iter()
            .filter(|r| !r.safety.is_tainted())
            .map(|r| r.cache_key.clone())
            .collect();

        let hits = cas.lookup_passing_hits(
            run_cfg.role,
            &graph.project_fingerprint,
            &cas_env_fp,
            &cacheable_keys,
            true,
        )?;

        for rec in selected_key_records {
            *static_total_by_file
                .entry(rec.file_path.clone())
                .or_insert(0) += 1;
            if !rec.safety.is_tainted()
                && let Some(hit) = hits.get(&rec.cache_key)
            {
                *static_cached_by_file
                    .entry(rec.file_path.clone())
                    .or_insert(0) += 1;
                cached_base_nodeids.insert(rec.nodeid.clone());
                exclude_by_file
                    .entry(rec.file_path.clone())
                    .or_default()
                    .push(rec.nodeid.clone());
                cached_records.push(TestRecord {
                    nodeid: rec.nodeid.clone(),
                    outcome: TestOutcome::Cached,
                    setup_ms: 0.0,
                    call_ms: 0.0,
                    teardown_ms: 0.0,
                    total_ms: 0.0,
                    worker_id: "cas-cache".to_owned(),
                    lane: ResourceLane::Hermetic,
                    fast_path: true,
                    attempts: 0,
                    longrepr: Some(format!("CAS hit (saved {:.1}ms)", hit.duration_ms)),
                });
                continue;
            }
            key_by_base_nodeid.insert(rec.nodeid.clone(), rec);
        }
    } else {
        for rec in selected_key_records {
            *static_total_by_file
                .entry(rec.file_path.clone())
                .or_insert(0) += 1;
            key_by_base_nodeid.insert(rec.nodeid.clone(), rec);
        }
    }

    // Build dynamic base-nodeid inventory per file from persisted traces so mixed files with
    // inherited/generated items NEVER get skipped as "100% cached" on Run 2!
    let mut dynamic_bases_by_file: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for tr in dynamic_idx.by_nodeid.values() {
        let f = normalize_rel_path(&tr.file_path);
        dynamic_bases_by_file
            .entry(f)
            .or_default()
            .insert(base_nodeid(&tr.nodeid));
    }

    // Decide which selected files STILL need pytest execution:
    // A file may only be skipped completely if:
    //   (a) `--cache` is enabled, AND
    //   (b) it has >= 1 static AST test node AND 100% of its static AST test nodes hit CAS, AND
    //   (c) its source has no custom subclassed `Test*` classes or dynamic test generators, AND
    //   (d) every dynamically observed base nodeid for this file (from prior runs) is in CAS hits!
    let mut files_to_run: Vec<String> = Vec::new();
    for file in &selected_files {
        let static_total = static_total_by_file.get(file).copied().unwrap_or(0);
        let static_cached = static_cached_by_file.get(file).copied().unwrap_or(0);
        let all_static_cached = opts.cache && static_total > 0 && static_cached == static_total;

        let may_have_non_static = tree
            .get(file)
            .and_then(|b| std::str::from_utf8(b).ok())
            .is_none_or(file_may_have_non_static_tests);

        let has_uncached_dynamic = dynamic_bases_by_file
            .get(file)
            .is_some_and(|bases| bases.iter().any(|b| !cached_base_nodeids.contains(b)));

        if all_static_cached && !may_have_non_static && !has_uncached_dynamic {
            continue;
        }
        files_to_run.push(file.clone());
    }

    let cached_count = cached_records.len();

    // 4. Execute ONLY files with uncached or potentially non-static tests (skipping workers
    //    completely when 100% of selected files are provably cached!).
    let mut report = if files_to_run.is_empty() {
        let mode_label = if opts.daemon {
            "warm-daemon+cas"
        } else {
            "cold+cas"
        };
        let mut rep = empty_report(
            mode_label,
            run_cfg.role,
            effective_tier,
            0,
            "all_cached",
            t_total.elapsed().as_millis() as u64,
            0,
        );
        rep.tests_selected = tests_selected;
        rep.tests_cached = cached_count;
        rep.tests_tainted = tests_tainted;
        rep.tia_parse_ms = tia_parse_ms;
        rep.tia_graph_ms = tia_graph_ms;
        rep.results = cached_records;
        rep.results.sort_by(|a, b| a.nodeid.cmp(&b.nodeid));
        if run_cfg.ndjson {
            emit_ndjson_from_report(&rep, &project_root);
        }
        rep
    } else {
        let mut exec_cfg = run_cfg.clone();
        exec_cfg.targets = files_to_run;
        exec_cfg.tier = effective_tier;
        exec_cfg.explicit_nodeids_by_file = None;
        // Automatically enable PEP 669 + PEP 578 + os.environ tracing when caching is active so
        // dynamic taints (subprocess.Popen/socket.connect), non-static nodeids, & env reads guard cache seeding!
        if opts.cache {
            exec_cfg.trace = true;
        }
        let emit_ndjson_after = exec_cfg.ndjson;
        if cached_count > 0 {
            exec_cfg.ndjson = false;
        }

        let exclude_arg = (!exclude_by_file.is_empty()).then_some(&exclude_by_file);
        let mut rep = if opts.daemon {
            execute_via_daemon_with_exclusions(&exec_cfg, exclude_arg).await?
        } else {
            execute_cold_run_with_exclusions(&exec_cfg, exclude_arg).await?
        };

        rep.tia_parse_ms = tia_parse_ms;
        rep.tia_graph_ms = tia_graph_ms;
        rep.tests_selected = tests_selected.max(rep.total_collected + cached_count);
        rep.tests_cached = cached_count;
        rep.tests_tainted = tests_tainted;
        rep.tier = effective_tier;

        if !cached_records.is_empty() {
            rep.results.extend(cached_records);
            rep.results.sort_by(|a, b| a.nodeid.cmp(&b.nodeid));
        }

        if emit_ndjson_after && (opts.daemon || cached_count > 0) {
            emit_ndjson_from_report(&rep, &project_root);
        }

        rep
    };

    // 5. Seed ONLY verified all-passing, non-tainted base nodeid families back into CasTestCache!
    let mut cache_seeded = 0usize;
    if let Some(ref mut cas) = cas_opt
        && !report.results.is_empty()
    {
        let mut dynamic_taint_by_base: BTreeSet<String> = BTreeSet::new();
        for tr in &report.traces {
            if !tr.taint_reasons.is_empty() {
                dynamic_taint_by_base.insert(base_nodeid(&tr.nodeid));
            }
        }

        // If new traces discovered `env_vars_read` or `read_assets`, recompute keys using the
        // EXACT same `base_graph_cfg` (preserving `django_settings_module`!) so lookup and seed
        // `env_fingerprint`s always match 100%!
        if !report.traces.is_empty() {
            let new_trace_bundle = DynamicTraceFile {
                schema_version: 1,
                python_version: String::new(),
                monitoring_backend: "pyrn-worker.sys.monitoring+audit+environ".to_owned(),
                total_wall_ms: report.wall_ms as f64,
                tests: report.traces.clone(),
            };
            dynamic_idx.merge(new_trace_bundle);

            let merged_list: Vec<DynamicTestTrace> =
                dynamic_idx.by_nodeid.values().cloned().collect();
            let _ = write_trace_file(&persisted_trace_path, &merged_list, report.wall_ms as f64);

            let (re_cfg, _) = base_graph_cfg.clone().effective_for_tier(effective_tier);
            let updated_graph = DependencyGraph::build(&tree, Some(&dynamic_idx), re_cfg);
            let updated_keys = updated_graph.compute_all_cache_keys(&env_map, effective_tier);
            for rec in updated_keys {
                if key_by_base_nodeid.contains_key(&rec.nodeid) {
                    key_by_base_nodeid.insert(rec.nodeid.clone(), rec);
                }
            }
        }

        // Aggregate executed outcomes per base nodeid (handling parametrized families soundly).
        let mut family_outcomes: BTreeMap<String, (usize, usize, f64)> = BTreeMap::new();
        for r in &report.results {
            if r.outcome == TestOutcome::Cached {
                continue;
            }
            let base = base_nodeid(&r.nodeid);
            let entry = family_outcomes.entry(base).or_insert((0, 0, 0.0));
            if r.outcome == TestOutcome::Passed {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
            entry.2 += r.total_ms;
        }

        let commit_sha = opts
            .commit_sha
            .clone()
            .unwrap_or_else(|| "working-tree".to_owned());
        let mut to_seed = Vec::new();
        for (base_nid, (passed_n, non_passed_n, dur_ms)) in family_outcomes {
            if passed_n == 0 || non_passed_n > 0 {
                continue;
            }
            if dynamic_taint_by_base.contains(&base_nid) {
                continue;
            }
            let Some(key_rec) = key_by_base_nodeid.get(&base_nid) else {
                continue;
            };
            if key_rec.safety.is_tainted() {
                continue;
            }
            to_seed.push(CachedTestEntry {
                cache_key: key_rec.cache_key.clone(),
                role: run_cfg.role,
                project_fp: graph.project_fingerprint.clone(),
                env_fingerprint: cas_env_fp.clone(),
                nodeid: base_nid,
                outcome: "passed".to_owned(),
                duration_ms: dur_ms,
                commit_sha: commit_sha.clone(),
                witness_128b: "pass".to_owned(),
                confidence_tier: effective_tier.as_str().to_owned(),
                created_at_epoch_s: 0,
                hit_count: 0,
            });
        }
        if !to_seed.is_empty() {
            cache_seeded = cas.put_passing_batch(&to_seed)?;
        }
    }

    if let Some(ref trace_out) = opts.trace_out
        && !report.traces.is_empty()
    {
        write_trace_file(trace_out, &report.traces, report.wall_ms as f64)?;
    }

    report.wall_ms = t_total.elapsed().as_millis() as u64;

    Ok(UnifiedRunOutcome {
        report,
        impact: impact_opt,
        env_fingerprint: graph.env_fingerprint,
        cache_seeded,
        resource_budget,
    })
}

fn reverse_reachable_from_changes(
    graph: &DependencyGraph,
    changed_specs: &[ChangedFileSpec],
) -> BTreeSet<String> {
    let mut visited = BTreeSet::new();
    let mut queue = std::collections::VecDeque::new();
    for spec in changed_specs {
        let p = normalize_rel_path(&spec.path);
        if visited.insert(p.clone()) {
            queue.push_back(p);
        }
    }
    while let Some(cur) = queue.pop_front() {
        if let Some(dependents) = graph.reverse_deps.get(&cur) {
            for dep in dependents {
                if visited.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }
    visited
}

fn collect_normalized_env() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (k, v) in std::env::vars() {
        #[cfg(windows)]
        {
            map.insert(k.to_ascii_uppercase(), v.clone());
        }
        map.insert(k, v);
    }
    map
}

fn normalize_env_map(src: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (k, v) in src {
        #[cfg(windows)]
        {
            map.insert(k.to_ascii_uppercase(), v.clone());
        }
        map.insert(k.clone(), v.clone());
    }
    map
}

const KNOWN_PURE_TEST_BASE_CLASSES: &[&str] = &[
    "object",
    "TestCase",
    "SimpleTestCase",
    "TransactionTestCase",
    "LiveServerTestCase",
    "unittest.TestCase",
    "django.test.TestCase",
    "django.test.SimpleTestCase",
    "django.test.TransactionTestCase",
];

/// Conservative static check: returns `true` if a test file might collect tests not visible as
/// direct `def test_*` functions (inherited `unittest.TestCase` mixins, `pytest_generate_tests`,
/// `load_tests`, or custom collectors). When `true`, `--cache` will still collect the file and
/// exclude only verified CAS hits rather than skipping the file completely.
fn file_may_have_non_static_tests(source: &str) -> bool {
    if source.contains("pytest_generate_tests")
        || source.contains("pytest_pycollect_makeitem")
        || source.contains("def load_tests")
        || source.contains("TestSuite")
    {
        return true;
    }
    for line in source.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("class ")
            && let Some(open_paren) = rest.find('(')
        {
            let cls_name = rest[..open_paren].trim();
            if cls_name.starts_with("Test") {
                let after = &rest[open_paren + 1..];
                let bases_str = after.split(')').next().unwrap_or(after).trim();
                for base in bases_str.split(',') {
                    let b = base.trim();
                    if b.is_empty() {
                        continue;
                    }
                    if !KNOWN_PURE_TEST_BASE_CLASSES.contains(&b) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn compute_cas_env_fingerprint(
    base_env_fp: &str,
    markers: Option<&str>,
    keyword: Option<&str>,
) -> String {
    if markers.is_none() && keyword.is_none() {
        return base_env_fp.to_owned();
    }
    let mut h = blake3::Hasher::new();
    h.update(b"pyrn.cas-env.v1\0");
    h.update(base_env_fp.as_bytes());
    h.update(b"\0m=");
    if let Some(m) = markers {
        h.update(m.as_bytes());
    }
    h.update(b"\0k=");
    if let Some(k) = keyword {
        h.update(k.as_bytes());
    }
    h.finalize().to_hex().to_string()
}

fn matches_targets(project_root: &Path, rel_file: &str, targets: &[String]) -> bool {
    let norm_file = normalize_rel_path(rel_file);
    for raw in targets {
        let p = PathBuf::from(raw);
        let rel_target = if p.is_absolute() {
            to_rel_posix(project_root, &p)
        } else {
            normalize_rel_path(raw)
        };
        let trimmed = rel_target.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "." {
            return true;
        }
        if norm_file == trimmed || norm_file.starts_with(&format!("{trimmed}/")) {
            return true;
        }
    }
    false
}

fn write_trace_file(path: &Path, traces: &[DynamicTestTrace], total_wall_ms: f64) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = DynamicTraceFile {
        schema_version: 1,
        python_version: "3.12".to_owned(),
        monitoring_backend: "pyrn-worker.sys.monitoring+audit+environ".to_owned(),
        total_wall_ms,
        tests: traces.to_vec(),
    };
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(&payload)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
