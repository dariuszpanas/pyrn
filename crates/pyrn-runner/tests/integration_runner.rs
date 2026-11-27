//! End-to-end integration tests for `pyrn-runner`:
//! - `pyproject.toml` `python_files = ["test_*.py", "*_test.py"]` discovery
//! - Hybrid Direct Fast-Path parity (`indirect=True`, zero-arg autouse error, opt-in `.pyrn.toml` `pure_autouse_guards`)
//! - Watchdog hang isolation, `os._exit(139)` crash recovery, flake detection, and crash-during-flake-rerun respawn
//! - Embedded PEP 669 + PEP 578 + `os.environ` dynamic tracing (`env_vars_read`)
//! - Unified `--tia --cache` pipeline (CAS hit/miss, `SlotRole` segregation, env-var invalidation, 0-worker cached run)
//! - Built-in `--cov-sysmon` isolated coverage capture + clean merge
//! - `pyrn doctor` (`collect_doctor_report`)

use pyrn_runner::{
    ConfidenceTier, ExecVerdict, RunConfig, SlotRole, TestOutcome, UnifiedRunOptions,
    collect_doctor_report, execute_cold_run, execute_unified_run, execute_unified_run_with_env,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn find_test_python() -> Option<String> {
    if let Ok(p) = std::env::var("PYRN_TEST_PYTHON")
        && !p.is_empty()
    {
        return Some(p);
    }
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        let base = PathBuf::from(venv);
        for rel in [
            Path::new("Scripts").join("python.exe"),
            Path::new("bin").join("python"),
        ] {
            let cand = base.join(rel);
            if cand.is_file() {
                candidates.push(cand.to_string_lossy().into_owned());
            }
        }
    }
    candidates.push("python".to_owned());
    candidates.push("python3".to_owned());
    for cand in candidates {
        if (cand.contains('\\') || cand.contains('/')) && !Path::new(&cand).is_file() {
            continue;
        }
        let ok = std::process::Command::new(&cand)
            .args(["-c", "import pytest"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return Some(cand);
        }
    }
    None
}

fn resilience_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests_resilience")
}

#[tokio::test]
async fn discovers_custom_suffix_and_preserves_indirect_and_fast_path_parity() {
    let Some(python) = find_test_python() else {
        return;
    };
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let cfg = RunConfig {
        project_root: resilience_dir(),
        python_exe: python,
        targets: vec![".".to_owned()],
        workers: Some(2),
        role: SlotRole::Agent,
        cache_dir: Some(cache_dir.path().to_path_buf()),
        test_timeout_secs: 20,
        fast_path: true,
        trace: true,
        trace_granularity: "test".to_owned(),
        ..RunConfig::default()
    };

    let rep = execute_cold_run(&cfg).await.expect("cold run");
    assert_eq!(rep.exit_code, 0, "unexpected failures: {:?}", rep.results);
    assert_eq!(rep.verdict, ExecVerdict::Pass);

    // 1. Both `test_*.py` AND `custom_naming_test.py` (`*_test.py`) were discovered and passed!
    assert!(
        rep.results
            .iter()
            .any(|r| r.nodeid.contains("custom_naming_test.py") && r.outcome == TestOutcome::Passed),
        "expected *_test.py to be discovered: {:?}",
        rep.results
    );

    // 2. `@pytest.mark.parametrize("doubled", [5, 7], indirect=True)` ran through full pytest (NOT fast_path)!
    let indirect_recs: Vec<_> = rep
        .results
        .iter()
        .filter(|r| {
            r.nodeid
                .contains("test_indirect_parametrize_fixture_is_never_bypassed")
        })
        .collect();
    assert_eq!(indirect_recs.len(), 2);
    for r in indirect_recs {
        assert_eq!(r.outcome, TestOutcome::Passed);
        assert!(
            !r.fast_path,
            "indirect=True must never use direct fast-path"
        );
    }

    // 3. Pure direct parametrize tests used fast_path and passed!
    let pure_recs: Vec<_> = rep
        .results
        .iter()
        .filter(|r| r.nodeid.contains("test_pure_direct_parametrize_fast_path"))
        .collect();
    assert_eq!(pure_recs.len(), 3);
    for r in pure_recs {
        assert_eq!(r.outcome, TestOutcome::Passed);
        assert!(r.fast_path, "pure parametrized test should use fast-path");
    }

    // 4. `os.environ` read proxy captured BOTH present/defaulted and absent env vars!
    let env_trace = rep
        .traces
        .iter()
        .find(|t| t.nodeid.contains("test_reads_feature_env_var"))
        .expect("dynamic trace for env probe test");
    assert!(
        env_trace
            .env_vars_read
            .keys()
            .any(|k| k.eq_ignore_ascii_case("PYRN_TEST_ENV_PROBE")),
        "expected PYRN_TEST_ENV_PROBE in env_vars_read, got {:?}",
        env_trace.env_vars_read
    );
    assert!(
        env_trace
            .env_vars_read
            .keys()
            .any(|k| k.eq_ignore_ascii_case("PYRN_TEST_ENV_ABSENT")),
        "expected PYRN_TEST_ENV_ABSENT in env_vars_read, got {:?}",
        env_trace.env_vars_read
    );
}

#[tokio::test]
async fn unified_tia_and_cas_cache_seeds_serves_hits_and_isolates_roles() {
    let Some(python) = find_test_python() else {
        return;
    };
    let repo = tempfile::tempdir().expect("temp repo");
    let cache_dir = tempfile::tempdir().expect("cache dir");

    fs::write(
        repo.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\npython_files = [\"test_*.py\", \"*_test.py\"]\n",
    )
    .unwrap();
    fs::write(
        repo.path().join(".pyrn.toml"),
        "pure_autouse_guards = [\"_opt_in_pure_guard\"]\n",
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("pkg")).unwrap();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::write(repo.path().join("pkg/__init__.py"), "").unwrap();
    fs::write(
        repo.path().join("pkg/math_ops.py"),
        "def add(a, b):\n    return a + b\n\ndef mul(a, b):\n    return a * b\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("tests/conftest.py"),
        "import pytest\n\n@pytest.fixture(autouse=True)\ndef _opt_in_pure_guard(request):\n    return None\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("tests/test_add.py"),
        "from pkg.math_ops import add\n\ndef test_add_basic():\n    assert add(2, 3) == 5\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("tests/mul_test.py"),
        "from pkg.math_ops import mul\n\ndef test_mul_basic():\n    assert mul(3, 4) == 12\n",
    )
    .unwrap();

    // Run 1 (Agent role): cold + cache -> executes both tests (fast-path unlocked by opt-in pure_autouse_guards!) and seeds CAS!
    let opts_agent = UnifiedRunOptions {
        run: RunConfig {
            project_root: repo.path().to_path_buf(),
            python_exe: python.clone(),
            workers: Some(2),
            role: SlotRole::Agent,
            tier: ConfidenceTier::StrictImpacted,
            cache_dir: Some(cache_dir.path().to_path_buf()),
            test_timeout_secs: 20,
            fast_path: true,
            ..RunConfig::default()
        },
        daemon: false,
        tia: false,
        cache: true,
        ..UnifiedRunOptions::default()
    };
    let out1 = execute_unified_run(&opts_agent).await.expect("run 1");
    assert_eq!(out1.report.exit_code, 0);
    assert_eq!(out1.report.passed, 2);
    assert_eq!(out1.report.tests_cached, 0);
    assert_eq!(
        out1.report.fast_path_tests, 2,
        "opt-in pure_autouse_guards should unlock fast-path"
    );
    assert_eq!(out1.cache_seeded, 2);

    // Run 2 (Agent role, unchanged): 100% CAS cache hit -> ZERO workers spawned!
    let out2 = execute_unified_run(&opts_agent).await.expect("run 2");
    assert_eq!(out2.report.exit_code, 0);
    assert_eq!(out2.report.tests_cached, 2);
    assert_eq!(out2.report.workers_requested, 0);
    assert_eq!(out2.report.worker_limit_reason, "all_cached");

    // Run 3 (Verify role): strictly isolated from Agent cache -> must execute both tests fresh!
    let mut opts_verify = opts_agent.clone();
    opts_verify.run.role = SlotRole::Verify;
    let out3 = execute_unified_run(&opts_verify)
        .await
        .expect("run 3 verify");
    assert_eq!(out3.report.exit_code, 0);
    assert_eq!(
        out3.report.tests_cached, 0,
        "verify role must NOT read agent cache"
    );
    assert_eq!(out3.report.passed, 2);
    assert_eq!(out3.cache_seeded, 2);
}

#[tokio::test]
async fn cov_sysmon_and_doctor_work_cleanly_without_touching_repo() {
    let Some(python) = find_test_python() else {
        return;
    };
    let has_cov = std::process::Command::new(&python)
        .args(["-c", "import coverage"])
        .status()
        .is_ok_and(|s| s.success());
    if !has_cov {
        return;
    }

    let cache_dir = tempfile::tempdir().expect("cache dir");
    let cov_out = cache_dir.path().join("merged.coverage");

    let cfg = RunConfig {
        project_root: resilience_dir(),
        python_exe: python.clone(),
        targets: vec!["custom_naming_test.py".to_owned()],
        workers: Some(1),
        role: SlotRole::Agent,
        cache_dir: Some(cache_dir.path().to_path_buf()),
        test_timeout_secs: 20,
        cov_sysmon: true,
        cov_out: Some(cov_out.clone()),
        ..RunConfig::default()
    };
    let rep = execute_cold_run(&cfg).await.expect("cov run");
    assert_eq!(rep.exit_code, 0);
    assert_eq!(rep.passed, 1);
    assert!(
        cov_out.is_file(),
        "expected merged sysmon coverage at {}",
        cov_out.display()
    );
    assert!(rep.coverage_file.is_some());

    let doc = collect_doctor_report(
        &resilience_dir(),
        &python,
        SlotRole::Agent,
        Some(cache_dir.path()),
    )
    .await
    .expect("doctor");
    assert!(doc.python_version.is_some());
    assert!(doc.pytest_version.is_some());
    assert!(doc.worker_budget.selected_workers >= 1);
    assert!(doc.worker_budget_explanation.contains("workers="));
}

#[tokio::test]
async fn warm_daemon_handles_same_size_edit_and_deleted_test_file_invalidation() {
    use pyrn_runner::{
        DaemonResponse, DaemonServeConfig, daemon_status, daemon_stop, execute_via_daemon,
        serve_daemon,
    };

    let Some(python) = find_test_python() else {
        return;
    };
    let repo = tempfile::tempdir().expect("temp repo");
    let cache_dir = tempfile::tempdir().expect("cache dir");

    fs::write(
        repo.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\npython_files = [\"test_*.py\", \"*_test.py\"]\n",
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    let edit_path = repo.path().join("tests/test_edit.py");
    let del_path = repo.path().join("tests/del_test.py");
    fs::write(&edit_path, "def test_v():\n    assert 1 == 1\n").unwrap();
    fs::write(&del_path, "def test_d():\n    assert 2 == 2\n").unwrap();

    let daemon_cfg = DaemonServeConfig {
        project_root: repo.path().to_path_buf(),
        python_exe: python.clone(),
        config_file: None,
        markers: None,
        workers: Some(2),
        role: SlotRole::Agent,
        cache_dir: Some(cache_dir.path().to_path_buf()),
        sqlite_template: false,
        extra_pytest_args: Vec::new(),
    };
    let server_handle = tokio::spawn(async move {
        let _ = serve_daemon(daemon_cfg).await;
    });

    // Wait until daemon is ready (bounded poll up to 15s).
    let mut ready = false;
    for _ in 0..60 {
        if let Ok(DaemonResponse::Status { workers_total, .. }) = daemon_status(
            repo.path(),
            &python,
            SlotRole::Agent,
            Some(cache_dir.path()),
        )
        .await
            && workers_total == 2
        {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(ready, "warm daemon failed to start");

    let run_cfg = RunConfig {
        project_root: repo.path().to_path_buf(),
        python_exe: python.clone(),
        targets: vec!["tests".to_owned()],
        workers: Some(2),
        role: SlotRole::Agent,
        cache_dir: Some(cache_dir.path().to_path_buf()),
        test_timeout_secs: 20,
        ..RunConfig::default()
    };

    // Run 1: both pass!
    let r1 = execute_via_daemon(&run_cfg).await.expect("daemon run 1");
    assert_eq!(r1.exit_code, 0);
    assert_eq!(r1.passed, 2);

    // Same-size edit (`1 == 1` -> `1 == 2`) AND delete `del_test.py`!
    fs::write(&edit_path, "def test_v():\n    assert 1 == 2\n").unwrap();
    fs::remove_file(&del_path).unwrap();

    // Run 2: MUST observe the edit (fail `test_v`) AND drop deleted `del_test.py` (total=1)!
    let r2 = execute_via_daemon(&run_cfg).await.expect("daemon run 2");
    assert_eq!(r2.total_collected, 1);
    assert_eq!(r2.failed, 1);
    assert_eq!(r2.exit_code, 1);

    let stop = daemon_stop(
        repo.path(),
        &python,
        SlotRole::Agent,
        Some(cache_dir.path()),
    )
    .await
    .expect("daemon stop");
    assert!(matches!(stop, DaemonResponse::Stopped));
    let _ = server_handle.await;
}

#[tokio::test]
async fn crash_during_flake_rerun_respawns_worker_cleanly() {
    let Some(python) = find_test_python() else {
        return;
    };
    let repo = tempfile::tempdir().expect("temp repo");
    let cache_dir = tempfile::tempdir().expect("cache dir");

    fs::write(
        repo.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\ntestpaths = [\".\"]\n",
    )
    .unwrap();
    // Always crashes with os._exit(139) so both initial run AND flake rerun attempt crash!
    fs::write(
        repo.path().join("test_rerun_crash.py"),
        "import os\n\ndef test_always_crashes():\n    os._exit(139)\n\ndef test_healthy():\n    assert True\n",
    )
    .unwrap();

    let cfg = RunConfig {
        project_root: repo.path().to_path_buf(),
        python_exe: python,
        targets: vec!["test_rerun_crash.py".to_owned()],
        workers: Some(1),
        role: SlotRole::Agent,
        cache_dir: Some(cache_dir.path().to_path_buf()),
        test_timeout_secs: 15,
        reruns: 1,
        ..RunConfig::default()
    };

    let rep = execute_cold_run(&cfg).await.expect("crash rerun");
    assert_eq!(rep.exit_code, 1);
    assert_eq!(rep.crashed, 1);
    assert_eq!(
        rep.passed, 1,
        "healthy sibling test MUST still be requeued and pass"
    );
    assert!(
        rep.worker_restarts >= 2,
        "expected >=2 restarts (initial crash + flake rerun crash), got {}",
        rep.worker_restarts
    );
    let crashed_rec = rep
        .results
        .iter()
        .find(|r| r.nodeid.contains("test_always_crashes"))
        .expect("crashed record");
    assert_eq!(crashed_rec.attempts, 2);
    assert_eq!(crashed_rec.outcome, TestOutcome::Crashed);
}

#[tokio::test]
async fn inherited_unittest_and_mixed_static_never_dropped_under_tia_or_cache() {
    let Some(python) = find_test_python() else {
        return;
    };
    let repo = tempfile::tempdir().expect("temp repo");
    let cache_dir = tempfile::tempdir().expect("cache dir");

    fs::write(
        repo.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::write(
        repo.path().join("tests/base_helpers.py"),
        "import unittest\n\nclass BaseCheck(unittest.TestCase):\n    def test_inherited_assertion(self):\n        assert False, 'inherited failure MUST run'\n",
    )
    .unwrap();
    // Pure inherited file: ZERO static `def test_*` functions!
    fs::write(
        repo.path().join("tests/test_child.py"),
        "from tests.base_helpers import BaseCheck\n\nclass TestChild(BaseCheck):\n    pass\n",
    )
    .unwrap();
    // Mixed file: 1 static passing test + 1 inherited failing test!
    fs::write(
        repo.path().join("tests/test_mixed.py"),
        "from tests.base_helpers import BaseCheck\n\ndef test_static_ok():\n    assert 1 + 1 == 2\n\nclass TestMixedChild(BaseCheck):\n    pass\n",
    )
    .unwrap();

    // 1. Pure inherited file under `--cache`: MUST fail with exit_code=1 (never false-positive exit=0)!
    let pure_opts = UnifiedRunOptions {
        run: RunConfig {
            project_root: repo.path().to_path_buf(),
            python_exe: python.clone(),
            targets: vec!["tests/test_child.py".to_owned()],
            workers: Some(1),
            role: SlotRole::Agent,
            tier: ConfidenceTier::StrictImpacted,
            cache_dir: Some(cache_dir.path().to_path_buf()),
            test_timeout_secs: 20,
            ..RunConfig::default()
        },
        daemon: false,
        tia: false,
        cache: true,
        ..UnifiedRunOptions::default()
    };
    let pure_out = execute_unified_run(&pure_opts)
        .await
        .expect("pure inherited --cache");
    assert_eq!(pure_out.report.exit_code, 1);
    assert_eq!(
        pure_out.report.failed, 2,
        "pytest collects both imported BaseCheck and TestChild (2 failures)"
    );

    // 2. Pure inherited file under `--tia --cache` when `base_helpers.py` changes: MUST fail!
    let mut tia_opts = pure_opts.clone();
    tia_opts.tia = true;
    tia_opts.changed_files = vec!["tests/base_helpers.py".to_owned()];
    let tia_out = execute_unified_run(&tia_opts)
        .await
        .expect("pure inherited --tia --cache");
    assert_eq!(tia_out.report.exit_code, 1);
    assert_eq!(tia_out.report.failed, 2);

    // 3. Mixed file Run 1 & Run 2 under `--cache`:
    //    Run 1 runs all -> static passes (seeded), inherited fails (2 failures)!
    //    Run 2 serves static from CAS (`tests_cached == 1`) AND STILL runs inherited failing tests (`failed == 2, exit=1`)!
    let mut mixed_opts = pure_opts;
    mixed_opts.run.targets = vec!["tests/test_mixed.py".to_owned()];
    let m1 = execute_unified_run(&mixed_opts).await.expect("mixed run 1");
    assert_eq!(m1.report.exit_code, 1);
    assert_eq!(m1.report.passed, 1);
    assert_eq!(m1.report.failed, 2);
    assert_eq!(m1.cache_seeded, 1);

    let m2 = execute_unified_run(&mixed_opts).await.expect("mixed run 2");
    assert_eq!(
        m2.report.exit_code, 1,
        "Run 2 MUST NOT skip inherited failure"
    );
    assert_eq!(
        m2.report.tests_cached, 1,
        "static test should be served from CAS"
    );
    assert_eq!(
        m2.report.failed, 2,
        "inherited failing tests must still execute and fail"
    );
}

#[tokio::test]
async fn django_settings_module_preserves_cas_hits_across_runs() {
    let Some(python) = find_test_python() else {
        return;
    };
    let repo = tempfile::tempdir().expect("temp repo");
    let cache_dir = tempfile::tempdir().expect("cache dir");

    fs::write(
        repo.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::write(
        repo.path().join("tests/test_env_cas.py"),
        "def test_one():\n    assert 2 + 2 == 4\n\ndef test_two():\n    assert 'ok'.upper() == 'OK'\n",
    )
    .unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "DJANGO_SETTINGS_MODULE".to_owned(),
        "testproject.settings".to_owned(),
    );

    let opts = UnifiedRunOptions {
        run: RunConfig {
            project_root: repo.path().to_path_buf(),
            python_exe: python,
            workers: Some(1),
            role: SlotRole::Agent,
            tier: ConfidenceTier::StrictImpacted,
            cache_dir: Some(cache_dir.path().to_path_buf()),
            test_timeout_secs: 20,
            ..RunConfig::default()
        },
        daemon: false,
        tia: false,
        cache: true,
        ..UnifiedRunOptions::default()
    };

    let r1 = execute_unified_run_with_env(&opts, Some(&env))
        .await
        .expect("run 1 with DJANGO_SETTINGS_MODULE");
    assert_eq!(r1.report.exit_code, 0);
    assert_eq!(r1.report.passed, 2);
    assert_eq!(r1.cache_seeded, 2);

    let r2 = execute_unified_run_with_env(&opts, Some(&env))
        .await
        .expect("run 2 with DJANGO_SETTINGS_MODULE");
    assert_eq!(r2.report.exit_code, 0);
    assert_eq!(
        r2.report.tests_cached, 2,
        "Run 2 with DJANGO_SETTINGS_MODULE must get 100% CAS hits"
    );
    assert_eq!(r2.report.workers_requested, 0);
}

#[tokio::test]
async fn multi_fault_flake_rerun_recovers_all_siblings_after_crash() {
    let Some(python) = find_test_python() else {
        return;
    };
    let repo = tempfile::tempdir().expect("temp repo");
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let marker = repo.path().join("flake_seen.marker");
    let marker_esc = marker.to_string_lossy().replace('\\', "/");

    fs::write(
        repo.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\ntestpaths = [\".\"]\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("test_all_four_faults.py"),
        format!(
            "import os, time\n\
             from pathlib import Path\n\n\
             def test_healthy_1():\n    assert 1 + 1 == 2\n\n\
             def test_hang():\n    time.sleep(30)\n\n\
             def test_healthy_2():\n    assert sum(range(5)) == 10\n\n\
             def test_crash_1():\n    os._exit(139)\n\n\
             def test_healthy_3():\n    assert 'ok'.upper() == 'OK'\n\n\
             def test_flake():\n    p = Path('{marker_esc}')\n    if not p.exists():\n        p.write_text('1')\n        raise AssertionError('transient flake')\n    assert True\n\n\
             def test_crash_2():\n    os._exit(139)\n"
        ),
    )
    .unwrap();

    let cfg = RunConfig {
        project_root: repo.path().to_path_buf(),
        python_exe: python,
        targets: vec!["test_all_four_faults.py".to_owned()],
        workers: Some(1),
        role: SlotRole::Agent,
        cache_dir: Some(cache_dir.path().to_path_buf()),
        test_timeout_secs: 2,
        reruns: 1,
        ..RunConfig::default()
    };

    let rep = execute_cold_run(&cfg).await.expect("4-fault storm");
    assert_eq!(rep.exit_code, 1);
    assert_eq!(rep.passed, 3, "all 3 healthy siblings must pass");
    assert_eq!(rep.timed_out, 1);
    assert_eq!(rep.crashed, 2);
    assert_eq!(
        rep.flaky, 1,
        "sibling flake MUST still be rerun after earlier crash!"
    );
    assert!(
        rep.worker_restarts >= 5,
        "expected >=5 restarts (hang + 2 initial crashes + 2 rerun crashes), got {}",
        rep.worker_restarts
    );

    let find = |needle: &str| {
        rep.results
            .iter()
            .find(|r| r.nodeid.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle}"))
    };
    assert_eq!(find("test_hang").outcome, TestOutcome::TimedOut);
    assert_eq!(find("test_hang").attempts, 1);
    assert_eq!(find("test_crash_1").outcome, TestOutcome::Crashed);
    assert_eq!(find("test_crash_1").attempts, 2);
    assert_eq!(find("test_flake").outcome, TestOutcome::Flaky);
    assert_eq!(find("test_flake").attempts, 2);
    assert_eq!(find("test_crash_2").outcome, TestOutcome::Crashed);
    assert_eq!(find("test_crash_2").attempts, 2);
}

#[tokio::test]
async fn cov_sysmon_captures_import_time_definitions_and_relative_cov_out() {
    let Some(python) = find_test_python() else {
        return;
    };
    let has_cov = std::process::Command::new(&python)
        .args(["-c", "import coverage"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !has_cov {
        return;
    }

    let repo = tempfile::tempdir().expect("temp repo");
    let cache_dir = tempfile::tempdir().expect("cache dir");

    fs::write(
        repo.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n\n[tool.coverage.run]\nsource = [\"pkg\"]\n",
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("pkg")).unwrap();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::write(repo.path().join("pkg/__init__.py"), "VERSION = '1.0'\n").unwrap();
    fs::write(
        repo.path().join("pkg/calc.py"),
        concat!(
            "FACTOR = 2\n\n",
            "class Multiplier:\n",
            "    def scale(self, x: int) -> int:\n",
            "        return x * FACTOR\n",
        ),
    )
    .unwrap();
    fs::write(
        repo.path().join("tests/test_calc.py"),
        concat!(
            "from pkg.calc import Multiplier\n\n",
            "def test_scale():\n",
            "    assert Multiplier().scale(21) == 42\n",
        ),
    )
    .unwrap();

    let cov_out = repo.path().join(".coverage");
    let cfg = RunConfig {
        project_root: repo.path().to_path_buf(),
        python_exe: python.clone(),
        targets: vec!["tests/test_calc.py".to_owned()],
        workers: Some(1),
        role: SlotRole::Verify,
        cache_dir: Some(cache_dir.path().to_path_buf()),
        cov_sysmon: true,
        cov_out: Some(PathBuf::from(".coverage")),
        test_timeout_secs: 20,
        ..RunConfig::default()
    };

    let rep = execute_cold_run(&cfg).await.expect("cov-sysmon run");
    assert_eq!(rep.exit_code, 0, "unexpected run report: {rep:?}");
    assert_eq!(rep.passed, 1, "unexpected run report: {rep:?}");
    assert!(cov_out.is_file(), "merged .coverage file must exist");

    let json_out = repo.path().join("cov.json");
    let status = std::process::Command::new(&python)
        .args(["-m", "coverage", "json", "-o", json_out.to_str().unwrap()])
        .current_dir(repo.path())
        .env("COVERAGE_FILE", &cov_out)
        .status()
        .expect("coverage json");
    assert!(status.success());

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&json_out).unwrap()).unwrap();
    let pct = parsed["totals"]["percent_covered"]
        .as_f64()
        .expect("percent_covered");
    assert!(
        pct >= 99.9,
        "import-time class/def/constant lines must be covered under --cov-sysmon, got {pct}%: {parsed}"
    );
}

#[tokio::test]
async fn tia_fails_closed_to_full_verified_when_git_base_is_missing() {
    let Some(python) = find_test_python() else {
        return;
    };

    let repo = tempfile::tempdir().expect("temp repo");
    let cache_dir = tempfile::tempdir().expect("cache dir");
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::write(
        repo.path().join("tests/test_alpha.py"),
        "def test_alpha():\n    assert 1 + 1 == 2\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("tests/test_beta.py"),
        "def test_beta():\n    assert 2 + 2 == 4\n",
    )
    .unwrap();

    // Directory is not even a git repository, so resolving `origin/main` must fail closed.
    let opts = UnifiedRunOptions {
        run: RunConfig {
            project_root: repo.path().to_path_buf(),
            python_exe: python,
            workers: Some(1),
            role: SlotRole::Verify,
            tier: ConfidenceTier::StrictImpacted,
            cache_dir: Some(cache_dir.path().to_path_buf()),
            test_timeout_secs: 20,
            ..RunConfig::default()
        },
        tia: true,
        base_ref: Some("origin/main".to_owned()),
        ..UnifiedRunOptions::default()
    };

    let outcome = execute_unified_run(&opts).await.expect("fail-closed run");
    let impact = outcome.impact.expect("impact metadata");
    assert_eq!(impact.effective_tier, ConfidenceTier::FullVerified);
    assert!(
        impact
            .full_fallback_reasons
            .iter()
            .any(|r| r.contains("fail-closed")),
        "expected fail-closed git fallback reason, got {:?}",
        impact.full_fallback_reasons
    );
    assert_eq!(outcome.report.exit_code, 0);
    assert_eq!(outcome.report.passed, 2);
}

#[tokio::test]
async fn tia_with_cache_bypasses_cas_reads_on_full_verified_fallback() {
    let Some(python) = find_test_python() else {
        return;
    };

    let repo = tempfile::tempdir().expect("temp repo");
    let cache_dir = tempfile::tempdir().expect("cache dir");
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::create_dir_all(repo.path().join("config")).unwrap();
    fs::write(repo.path().join("config/policy.yaml"), "strict: true\n").unwrap();
    fs::write(
        repo.path().join("tests/test_alpha.py"),
        "def test_alpha():\n    assert True\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("tests/test_beta.py"),
        "def test_beta():\n    assert True\n",
    )
    .unwrap();

    // 1. Seed the CAS cache with a clean full run.
    let seed_opts = UnifiedRunOptions {
        run: RunConfig {
            project_root: repo.path().to_path_buf(),
            python_exe: python.clone(),
            workers: Some(1),
            role: SlotRole::Verify,
            tier: ConfidenceTier::FullVerified,
            cache_dir: Some(cache_dir.path().to_path_buf()),
            test_timeout_secs: 20,
            ..RunConfig::default()
        },
        cache: true,
        ..UnifiedRunOptions::default()
    };
    let seeded = execute_unified_run(&seed_opts).await.expect("seed run");
    assert_eq!(seeded.report.passed, 2);
    assert_eq!(seeded.cache_seeded, 2);

    // 2. Simulate an unmapped non-Python file edit under `--tia --cache`:
    //    TIA fails closed to FullVerified and MUST bypass CAS reads so both tests run cold.
    fs::write(repo.path().join("config/policy.yaml"), "strict: false\n").unwrap();
    let fallback_opts = UnifiedRunOptions {
        run: RunConfig {
            project_root: repo.path().to_path_buf(),
            python_exe: python,
            workers: Some(1),
            role: SlotRole::Verify,
            tier: ConfidenceTier::StrictImpacted,
            cache_dir: Some(cache_dir.path().to_path_buf()),
            test_timeout_secs: 20,
            ..RunConfig::default()
        },
        tia: true,
        cache: true,
        changed_files: vec!["config/policy.yaml".to_owned()],
        ..UnifiedRunOptions::default()
    };
    let outcome = execute_unified_run(&fallback_opts)
        .await
        .expect("fallback run");
    let impact = outcome.impact.expect("impact metadata");
    assert_eq!(impact.effective_tier, ConfidenceTier::FullVerified);
    assert_eq!(
        outcome.report.tests_cached, 0,
        "FullVerified fallback must not serve stale CAS hits"
    );
    assert_eq!(outcome.report.passed, 2);
}

#[test]
fn git_changed_specs_uses_merge_base_when_base_branch_advances() {
    let repo = tempfile::tempdir().expect("temp git repo");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .output()
            .expect("git command");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run_git(&["init", "-b", "main"]);
    run_git(&["config", "user.email", "ci@example.com"]);
    run_git(&["config", "user.name", "CI"]);
    fs::create_dir_all(repo.path().join("pkg")).unwrap();
    fs::write(repo.path().join("pkg/a.py"), "def a():\n    return 1\n").unwrap();
    fs::write(repo.path().join("pkg/b.py"), "def b():\n    return 2\n").unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "initial"]);

    // Branch off main and edit only pkg/a.py.
    run_git(&["checkout", "-b", "feature"]);
    fs::write(repo.path().join("pkg/a.py"), "def a():\n    return 10\n").unwrap();
    run_git(&["add", "pkg/a.py"]);
    run_git(&["commit", "-m", "feature change"]);

    // Advance main independently with an edit to pkg/b.py.
    run_git(&["checkout", "main"]);
    fs::write(repo.path().join("pkg/b.py"), "def b():\n    return 20\n").unwrap();
    run_git(&["add", "pkg/b.py"]);
    run_git(&["commit", "-m", "main change"]);

    // Switch back to feature: diffing against main must use merge-base and see ONLY pkg/a.py.
    run_git(&["checkout", "feature"]);
    let cfg = pyrn_core::config::PyrnConfig::load_from_dir(repo.path());
    let specs = pyrn_core::git_replay::git_changed_specs_against_base(repo.path(), "main", &cfg)
        .expect("git specs");
    let paths: Vec<String> = specs.into_iter().map(|s| s.path).collect();
    assert_eq!(paths, vec!["pkg/a.py".to_owned()]);
}
