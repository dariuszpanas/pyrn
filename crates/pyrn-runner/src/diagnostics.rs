//! Actionable slow-test/fixture/straggler profiling ("why was this run slow?"),
//! `pyrn doctor` environment & container budget inspection, key-value metrics emission,
//! and `<= 2 KiB` `CompactReceipt` + `<= 128 B` `CrashCapsule`.

use crate::daemon::daemon_status;
use crate::discovery::compute_migrations_hash;
use crate::isolation::{CacheLayout, clean_canonical_path};
use crate::protocol::{
    CompactReceipt, CrashCapsule, DaemonResponse, RunMetrics, RunReport, SlotRole, StreamEvent,
    TestOutcome, TestRecord,
};
use crate::resources::{ResourceBudget, detect_worker_budget};
use crate::supervisor::emit_stream;
use anyhow::Result;
use pyrn_core::config::PyrnConfig;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// Build shared `pyrn_core::receipt::RunMetrics` from a `RunReport`.
#[must_use]
pub fn build_run_metrics(report: &RunReport) -> RunMetrics {
    let total_failures = report.failed + report.errors + report.timed_out + report.crashed;
    let total_denom = report
        .tests_selected
        .max(report.total_collected + report.tests_cached);
    let cache_hit_pct = (report.tests_cached * 100)
        .checked_div(total_denom)
        .unwrap_or(0) as i64;
    RunMetrics {
        tests_total: (report.total_collected + report.tests_cached) as i64,
        tests_selected: report.tests_selected as i64,
        tests_cached: report.tests_cached as i64,
        tests_tainted: report.tests_tainted as i64,
        tests_passed: report.passed as i64,
        tests_failed: total_failures as i64,
        tests_skipped: report.skipped as i64,
        tests_xfailed: report.xfailed as i64,
        tests_xpassed: report.xpassed as i64,
        tests_flaky: report.flaky as i64,
        tests_timed_out: report.timed_out as i64,
        tests_crashed: report.crashed as i64,
        fast_path_tests: report.fast_path_tests as i64,
        workers_selected: report.workers_requested as i64,
        worker_restarts: i64::from(report.worker_restarts),
        db_template_hits: i64::from(report.db_template_hits),
        cache_hit_pct,
        tia_parse_ms: report.tia_parse_ms as i64,
        tia_graph_ms: report.tia_graph_ms as i64,
        wall_ms: report.wall_ms as i64,
        discovery_ms: report.discovery_ms as i64,
        pool_ready_ms: report.pool_ready_ms as i64,
    }
}

/// Format deterministic `<prefix> key=<i64>` metrics line (including `workers_selected=<N>`).
#[must_use]
pub fn format_metrics_kv(report: &RunReport, prefix: &str) -> String {
    build_run_metrics(report).to_kv_line(prefix)
}

/// Print deterministic `<prefix> key=<i64>` metrics line to stdout.
pub fn emit_metrics_kv(report: &RunReport, prefix: &str) {
    println!("{}", format_metrics_kv(report, prefix));
}

/// Emit NDJSON `StreamEvent` replay for warm-daemon or cached runs.
pub fn emit_ndjson_from_report(report: &RunReport, project_root: &Path) {
    let unique_files: HashSet<&str> = report
        .results
        .iter()
        .map(|r| r.nodeid.split("::").next().unwrap_or(&r.nodeid))
        .collect();
    let units_scheduled: usize = report.worker_stats.iter().map(|w| w.units_run).sum();
    emit_stream(&StreamEvent::RunStarted {
        mode: report.mode.clone(),
        role: report.role,
        tier: report.tier,
        project_root: project_root.to_string_lossy().into_owned(),
        workers: report.workers_requested,
        files_discovered: unique_files.len(),
        units_scheduled,
        tests_cached: report.tests_cached,
    });
    for rec in &report.results {
        emit_stream(&StreamEvent::TestFinished {
            nodeid: rec.nodeid.clone(),
            outcome: rec.outcome,
            duration_ms: rec.total_ms,
            worker_id: rec.worker_id.clone(),
            fast_path: rec.fast_path,
        });
    }
    emit_stream(&StreamEvent::RunFinished {
        verdict: report.verdict,
        exit_code: report.exit_code,
        wall_ms: report.wall_ms,
        passed: report.passed,
        cached: report.tests_cached,
        failed: report.failed,
        skipped: report.skipped,
        flaky: report.flaky,
        timed_out: report.timed_out,
        crashed: report.crashed,
    });
}

/// Build a guaranteed `<= 2,048 B` compact receipt JSON + `<= 128 B` `CrashCapsule`.
#[must_use]
pub fn build_compact_receipt(
    report: &RunReport,
    env_fingerprint: &str,
) -> (CompactReceipt, Vec<u8>) {
    let total_failures = report.failed + report.errors + report.timed_out + report.crashed;

    let crash_capsule = report
        .results
        .iter()
        .find(|r| r.outcome.is_failure())
        .map(|r| {
            CrashCapsule::from_failure(
                "pyrn",
                &r.nodeid,
                r.longrepr.as_deref().unwrap_or(&r.nodeid),
            )
        })
        .or_else(|| {
            report
                .collection_errors
                .first()
                .map(|e| CrashCapsule::from_failure("pyrn-collect", "collection", e))
        });

    let summary = format!(
        "{} mode={} tier={} workers={}({}) passed={} cached={} failed={} skipped={} flaky={} wall={}ms",
        report.verdict.as_str(),
        report.mode,
        report.tier.as_str(),
        report.workers_requested,
        report.worker_limit_reason,
        report.passed,
        report.tests_cached,
        total_failures,
        report.skipped,
        report.flaky,
        report.wall_ms,
    );

    let receipt = CompactReceipt {
        schema: CompactReceipt::SCHEMA_V1.to_owned(),
        verdict: report.verdict,
        tier: report.tier,
        role: report.role,
        env_fingerprint: env_fingerprint[..16.min(env_fingerprint.len())].to_owned(),
        wall_ms: report.wall_ms,
        tests_total: report.total_collected + report.tests_cached,
        tests_selected: report.tests_selected,
        tests_cached: report.tests_cached,
        tests_tainted: report.tests_tainted,
        tests_passed: report.passed,
        tests_failed: total_failures,
        tests_skipped: report.skipped,
        tests_flaky: report.flaky,
        worker_restarts: report.worker_restarts,
        fast_path_tests: report.fast_path_tests,
        crash_capsule,
        summary,
    };

    let bytes = receipt.to_bounded_json();
    (receipt, bytes)
}

/// Print human-readable test summary + actionable "Why was this run slow?" diagnostics.
pub fn print_human_summary(report: &RunReport, top_slow: usize, show_failures: usize) {
    println!(
        "\n=== pyrn ({}, role={}, tier={}, workers={} [limit={}]) ===",
        report.mode,
        report.role.as_str(),
        report.tier.as_str(),
        report.workers_requested,
        report.worker_limit_reason,
    );
    println!(
        "Outcomes: {} passed, {} cached, {} failed, {} skipped, {} xfailed, {} xpassed, {} errors, {} timed_out, {} crashed, {} flaky (executed={}, deselected={})",
        report.passed,
        report.tests_cached,
        report.failed,
        report.skipped,
        report.xfailed,
        report.xpassed,
        report.errors,
        report.timed_out,
        report.crashed,
        report.flaky,
        report.total_collected,
        report.deselected,
    );
    println!(
        "Timing:   wall={:.2}s | discovery={}ms | tia={}ms | pool_ready={}ms | sum_collect={:.2}s | sum_db_setup={:.2}s (template_hits={}) | fast_path={}/{} ({:.1}%) | restarts={}",
        report.wall_ms as f64 / 1000.0,
        report.discovery_ms,
        report.tia_parse_ms + report.tia_graph_ms,
        report.pool_ready_ms,
        report.collect_total_ms / 1000.0,
        report.db_setup_total_ms / 1000.0,
        report.db_template_hits,
        report.fast_path_tests,
        report.total_collected,
        if report.total_collected > 0 {
            (report.fast_path_tests as f64 / report.total_collected as f64) * 100.0
        } else {
            0.0
        },
        report.worker_restarts,
    );
    if let Some(ref cov) = report.coverage_file {
        println!("Coverage: merged sysmon data -> {cov}");
    }

    let failures: Vec<&TestRecord> = report
        .results
        .iter()
        .filter(|r| r.outcome.is_failure() || r.outcome == TestOutcome::Flaky)
        .collect();
    if !failures.is_empty() && show_failures > 0 {
        println!(
            "\n--- Failures / Faults / Flakes ({} total) ---",
            failures.len()
        );
        for rec in failures.iter().take(show_failures) {
            println!(
                "  [{:?}] {} ({:.1}ms, worker={}, attempts={})",
                rec.outcome, rec.nodeid, rec.total_ms, rec.worker_id, rec.attempts
            );
            if let Some(ref lr) = rec.longrepr {
                for line in lr.lines().take(6) {
                    println!("      {line}");
                }
            }
        }
    }
    if !report.collection_errors.is_empty() {
        println!(
            "\n--- Collection Errors ({}) ---",
            report.collection_errors.len()
        );
        for err in report.collection_errors.iter().take(5) {
            for line in err.lines().take(8) {
                println!("  {line}");
            }
        }
    }

    if top_slow > 0 && !report.results.is_empty() {
        print_slow_diagnostics(report, top_slow);
    }
}

fn print_slow_diagnostics(report: &RunReport, top_n: usize) {
    let total_setup_ms: f64 = report.results.iter().map(|r| r.setup_ms).sum();
    let total_call_ms: f64 = report.results.iter().map(|r| r.call_ms).sum();
    let total_teardown_ms: f64 = report.results.iter().map(|r| r.teardown_ms).sum();
    let total_cpu_ms = total_setup_ms + total_call_ms + total_teardown_ms;

    let active_workers: Vec<f64> = report
        .worker_stats
        .iter()
        .filter(|w| w.units_run > 0 || w.tests_run > 0)
        .map(|w| w.wall_active_ms)
        .collect();

    let (median_w, max_w, straggler_id) = if !active_workers.is_empty() {
        let mut sorted = active_workers.clone();
        sorted.sort_by(f64::total_cmp);
        let med = sorted[sorted.len() / 2];
        let max_stat = report
            .worker_stats
            .iter()
            .max_by(|a, b| a.wall_active_ms.total_cmp(&b.wall_active_ms));
        (
            med,
            max_stat.map_or(0.0, |s| s.wall_active_ms),
            max_stat.map_or("-", |s| s.worker_id.as_str()),
        )
    } else {
        (0.0, 0.0, "-")
    };

    let parallel_eff = if report.wall_ms > 0 && !active_workers.is_empty() {
        let sum_active: f64 = active_workers.iter().sum();
        (sum_active / (report.wall_ms as f64 * active_workers.len() as f64)) * 100.0
    } else {
        0.0
    };

    println!("\n--- Why Was This Run Slow? (Diagnostics) ---");
    println!(
        "  CPU test breakdown: setup={:.2}s ({:.1}%) | call={:.2}s ({:.1}%) | teardown={:.2}s ({:.1}%)",
        total_setup_ms / 1000.0,
        pct(total_setup_ms, total_cpu_ms),
        total_call_ms / 1000.0,
        pct(total_call_ms, total_cpu_ms),
        total_teardown_ms / 1000.0,
        pct(total_teardown_ms, total_cpu_ms),
    );
    println!(
        "  Worker balance:     active_workers={} | median_active={:.2}s | max_active={:.2}s ({}) | tail_skew={:.2}s | efficiency={:.1}%",
        active_workers.len(),
        median_w / 1000.0,
        max_w / 1000.0,
        straggler_id,
        (max_w - median_w).max(0.0) / 1000.0,
        parallel_eff,
    );

    let mut by_test: Vec<&TestRecord> = report
        .results
        .iter()
        .filter(|r| r.outcome != TestOutcome::Cached)
        .collect();
    by_test.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
    println!("  Top {} slowest tests:", top_n.min(by_test.len()));
    for rec in by_test.iter().take(top_n) {
        println!(
            "    {:7.1}ms (setup={:6.1} call={:6.1} td={:5.1}) [{}] {}",
            rec.total_ms,
            rec.setup_ms,
            rec.call_ms,
            rec.teardown_ms,
            if rec.fast_path { "fast" } else { "pytest" },
            rec.nodeid
        );
    }

    let mut per_file: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for r in report
        .results
        .iter()
        .filter(|r| r.outcome != TestOutcome::Cached)
    {
        let f = r.nodeid.split("::").next().unwrap_or(&r.nodeid).to_owned();
        let entry = per_file.entry(f).or_insert((0.0, 0));
        entry.0 += r.total_ms;
        entry.1 += 1;
    }
    let mut file_vec: Vec<(String, f64, usize)> = per_file
        .into_iter()
        .map(|(f, (ms, n))| (f, ms, n))
        .collect();
    file_vec.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!(
        "  Top {} slowest files (sum of test CPU ms):",
        top_n.min(file_vec.len())
    );
    for (f, ms, n) in file_vec.iter().take(top_n) {
        println!("    {:7.1}ms ({:4} tests) {}", ms, n, f);
    }
}

/// Structured diagnostics returned by `pyrn doctor`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DoctorReport {
    pub project_root: String,
    pub python_exe: String,
    pub python_version: Option<String>,
    pub pytest_version: Option<String>,
    pub coverage_version: Option<String>,
    pub sys_monitoring_available: bool,
    pub role: SlotRole,
    pub worker_budget: ResourceBudget,
    pub worker_budget_explanation: String,
    pub testpaths: Vec<String>,
    pub python_files: Vec<String>,
    pub pure_autouse_guards: Vec<String>,
    pub cache_dir: String,
    pub cas_db_path: String,
    pub sqlite_template_path: String,
    pub sqlite_template_exists: bool,
    pub daemon_running: bool,
    pub daemon_pid: Option<u32>,
    pub daemon_workers: Option<usize>,
}

/// Collect full environment, container resource budget, Python/pytest/sysmon, and daemon health diagnostics.
pub async fn collect_doctor_report(
    project_root: &Path,
    python_exe: &str,
    role: SlotRole,
    cache_dir: Option<&Path>,
) -> Result<DoctorReport> {
    let root = clean_canonical_path(
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf()),
    );
    let cfg = PyrnConfig::load_from_dir(&root);
    let budget = detect_worker_budget(Some(&root), None, None);
    let mig_hash = compute_migrations_hash(&root, python_exe);
    let cache = CacheLayout::prepare(cache_dir, role, &root, python_exe, &mig_hash)?;

    let (python_version, pytest_version, coverage_version, sys_monitoring_available) =
        probe_python_env(python_exe).await;

    let (daemon_running, daemon_pid, daemon_workers) =
        match daemon_status(&root, python_exe, role, cache_dir).await {
            Ok(DaemonResponse::Status {
                pid, workers_total, ..
            }) => (true, Some(pid), Some(workers_total)),
            _ => (false, None, None),
        };

    Ok(DoctorReport {
        project_root: root.to_string_lossy().into_owned(),
        python_exe: python_exe.to_owned(),
        python_version,
        pytest_version,
        coverage_version,
        sys_monitoring_available,
        role,
        worker_budget_explanation: budget.explain(),
        worker_budget: budget,
        testpaths: cfg.testpaths,
        python_files: cfg.python_files,
        pure_autouse_guards: cfg.pure_autouse_guards,
        cache_dir: cache.runtime_dir.to_string_lossy().into_owned(),
        cas_db_path: cache.cas_db_path.to_string_lossy().into_owned(),
        sqlite_template_path: cache.sqlite_template_path.to_string_lossy().into_owned(),
        sqlite_template_exists: cache.sqlite_template_path.is_file(),
        daemon_running,
        daemon_pid,
        daemon_workers,
    })
}

async fn probe_python_env(
    python_exe: &str,
) -> (Option<String>, Option<String>, Option<String>, bool) {
    let script = "import json, sys\n\
        out = {'py': '.'.join(map(str, sys.version_info[:3])), 'sysmon': hasattr(sys, 'monitoring'), 'pytest': None, 'cov': None}\n\
        try:\n import pytest; out['pytest'] = pytest.__version__\n\
        except Exception: pass\n\
        try:\n import coverage; out['cov'] = coverage.__version__\n\
        except Exception: pass\n\
        print(json.dumps(out))";

    let Ok(output) = tokio::process::Command::new(python_exe)
        .arg("-c")
        .arg(script)
        .output()
        .await
    else {
        return (None, None, None, false);
    };
    if !output.status.success() {
        return (None, None, None, false);
    }
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return (None, None, None, false);
    };
    (
        val.get("py").and_then(|v| v.as_str()).map(str::to_owned),
        val.get("pytest")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        val.get("cov").and_then(|v| v.as_str()).map(str::to_owned),
        val.get("sysmon").and_then(|v| v.as_bool()).unwrap_or(false),
    )
}

fn pct(part: f64, whole: f64) -> f64 {
    if whole <= 0.0 {
        0.0
    } else {
        (part / whole) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        ConfidenceTier, DEFAULT_METRICS_PREFIX, ExecVerdict, MAX_CRASH_CAPSULE_BYTES,
        MAX_RECEIPT_BYTES, ResourceLane,
    };
    use crate::supervisor::empty_report;

    #[test]
    fn compact_receipt_and_run_metrics_respect_bounds_and_prefix() {
        let mut rep = empty_report(
            "cold",
            SlotRole::Verify,
            ConfidenceTier::StrictImpacted,
            16,
            "physical_cpus",
            1234,
            12,
        );
        rep.failed = 1;
        rep.total_collected = 100;
        rep.tests_selected = 100;
        rep.passed = 99;
        rep.verdict = ExecVerdict::Failed;
        rep.exit_code = 1;
        rep.results.push(TestRecord {
            nodeid: "tests/unit/test_long.py::test_something_that_failed".to_owned(),
            outcome: TestOutcome::Failed,
            setup_ms: 1.0,
            call_ms: 2.0,
            teardown_ms: 0.5,
            total_ms: 3.5,
            worker_id: "w0-hermetic".to_owned(),
            lane: ResourceLane::Hermetic,
            fast_path: true,
            attempts: 1,
            longrepr: Some("E   AssertionError: ".to_owned() + &"x".repeat(4000)),
        });

        let (receipt, bytes) = build_compact_receipt(&rep, &"a".repeat(64));
        assert!(bytes.len() <= MAX_RECEIPT_BYTES);
        assert_eq!(receipt.verdict, ExecVerdict::Failed);
        let cap = receipt.crash_capsule.expect("crash capsule");
        assert!(cap.summary.len() <= MAX_CRASH_CAPSULE_BYTES);

        let default_line = format_metrics_kv(&rep, DEFAULT_METRICS_PREFIX);
        assert!(default_line.starts_with("PYRN_METRIC "));
        assert!(default_line.contains("workers_selected=16"));

        let custom_line = format_metrics_kv(&rep, "CUSTOM_METRIC");
        assert!(custom_line.starts_with("CUSTOM_METRIC "));
        assert!(custom_line.contains("workers_selected=16"));
    }
}
