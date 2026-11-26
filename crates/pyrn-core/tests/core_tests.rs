//! Comprehensive soundness, symbol-level precision, in-memory policy, and CAS cache test suite for `pyrn-core`.
//!
//! Works under both `--no-default-features` (pure in-memory tests) and `--all-features` (incl. `cache` & `process`).

use std::collections::{BTreeMap, BTreeSet};

use pyrn_core::{
    CacheSafety, ChangedSymbols, CompactReceipt, ConfidenceTier, CrashCapsule,
    DEFAULT_METRICS_PREFIX, DependencyGraph, DynamicTestTrace, DynamicTraceFile, DynamicTraceIndex,
    ExecVerdict, GraphConfig, InMemoryTree, MAX_CRASH_CAPSULE_BYTES, MAX_RECEIPT_BYTES,
    PlanOptions, PyrnConfig, SlotRole, diff_python_symbols, parse_python_file, plan_python_tests,
};

fn build_synthetic_django_repo() -> InMemoryTree {
    let mut tree = InMemoryTree::default();
    tree.insert(
        "pyproject.toml",
        br#"
[project]
name = "sample-app"
[tool.hatch.build.targets.wheel]
packages = ["src/myapp"]
[tool.pytest.ini_options]
testpaths = ["tests"]
python_files = ["test_*.py", "*_test.py"]
"#
        .to_vec(),
    );
    tree.insert("uv.lock", b"version = 1\n".to_vec());
    tree.insert("src/myapp/__init__.py", b"__version__ = '0.1.0'\n".to_vec());
    tree.insert(
        "src/myapp/subpkg/__init__.py",
        b"from .core import compute\n".to_vec(),
    );
    tree.insert(
        "src/myapp/subpkg/core.py",
        b"from .helpers import helper_fn\ndef compute(x):\n    return helper_fn(x) + 1\n".to_vec(),
    );
    tree.insert(
        "src/myapp/subpkg/helpers.py",
        b"def helper_fn(x):\n    return x * 2\n".to_vec(),
    );
    tree.insert(
        "src/myapp/lazy_dep.py",
        b"def secret():\n    return 42\n".to_vec(),
    );
    tree.insert(
        "src/myapp/patched_target.py",
        b"def fetch():\n    return 'live'\n".to_vec(),
    );
    tree.insert("src/myapp/models.py", b"class Job:\n    pass\n".to_vec());
    tree.insert(
        "src/myapp/apps.py",
        br#"
class MyAppConfig:
    def ready(self):
        from myapp.conf.settings_helper import validate_settings
        validate_settings()
"#
        .to_vec(),
    );
    tree.insert("src/myapp/migrations/__init__.py", b"".to_vec());
    tree.insert(
        "src/myapp/migrations/0001_initial.py",
        b"operations = []\n".to_vec(),
    );
    tree.insert(
        "src/myapp/admin.py",
        b"class JobAdmin:\n    pass\n".to_vec(),
    );
    tree.insert(
        "src/myapp/management/commands/run_worker.py",
        b"def handle():\n    return True\n".to_vec(),
    );
    tree.insert(
        "src/myapp/workflows.py",
        b"_cached = None\ndef run_wf():\n    return 1\n".to_vec(),
    );
    tree.insert(
        "src/myapp/shared_defaults.py",
        b"DEFAULT_RETRIES = 3\n".to_vec(),
    );
    tree.insert(
        "src/myapp/result_storage.py",
        br#"
import os

_DEFAULT_PREFIX = "results"

class ResultStorageError(RuntimeError):
    pass

def _validate_prefix(cfg):
    return cfg.get("PREFIX", _DEFAULT_PREFIX)

def validate_storage_configuration(cfg):
    if not _validate_prefix(cfg):
        raise ResultStorageError("bad prefix")

class FilesystemResultStorage:
    def delete(self, key):
        return key
"#
        .to_vec(),
    );
    tree.insert(
        "src/myapp/conf/settings_helper.py",
        br#"
from myapp.shared_defaults import DEFAULT_RETRIES

TIMEOUT = 30

def validate_settings():
    from myapp.result_storage import ResultStorageError, validate_storage_configuration
    validate_storage_configuration({})
    return DEFAULT_RETRIES
"#
        .to_vec(),
    );
    tree.insert("testproject/urls.py", b"urlpatterns = []\n".to_vec());
    tree.insert(
        "testproject/templates/index.html",
        b"<h1>Hi</h1>\n".to_vec(),
    );
    tree.insert(".github/workflows/ci.yml", b"name: CI\n".to_vec());

    tree.insert(
        "tests/conftest.py",
        br#"
import pytest
from tests.harness_helper import shared_boot

@pytest.fixture()
def ray_cluster():
    yield "ray"

@pytest.fixture(autouse=True)
def _seed_db(request):
    database_fixtures = {"db", "transactional_db"}
    if not request.node.get_closest_marker("django_db"):
        return
    from myapp.models import Job

@pytest.fixture(autouse=True)
def _clear_caches():
    try:
        import myapp.workflows as wf
        wf._cached = None
    except ImportError:
        pass
"#
        .to_vec(),
    );
    tree.insert(
        "tests/harness_helper.py",
        b"def shared_boot():\n    return True\n".to_vec(),
    );

    tree.insert(
        "tests/unit/test_core.py",
        b"from myapp.subpkg import compute\ndef test_compute():\n    assert compute(2) == 5\n"
            .to_vec(),
    );
    tree.insert(
        "tests/unit/test_lazy_and_patch.py",
        br#"
from unittest.mock import patch
def test_lazy_and_patched():
    from myapp.lazy_dep import secret
    with patch("myapp.patched_target.fetch", return_value="mocked"):
        assert secret() == 42
"#
        .to_vec(),
    );
    tree.insert(
        "tests/integration/test_db_and_cmd.py",
        br#"
import pytest
from django.core.management import call_command

@pytest.mark.django_db
def test_worker_cmd():
    call_command("run_worker")
"#
        .to_vec(),
    );
    tree.insert(
        "tests/unit/test_ci_policy.py",
        br#"
from pathlib import Path
ROOT = Path(__file__).parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
def test_ci_exists():
    assert (WORKFLOWS / "ci.yml").exists()
"#
        .to_vec(),
    );
    tree.insert(
        "tests/unit/test_source_check.py",
        br#"
import inspect
from myapp.subpkg import helpers
def test_helper_source_comment():
    assert "helper_fn" in inspect.getsource(helpers)
"#
        .to_vec(),
    );
    tree.insert(
        "tests/integration/test_real_ray.py",
        br#"
import pytest
@pytest.mark.real_ray
def test_live_ray(ray_cluster):
    assert ray_cluster == "ray"
"#
        .to_vec(),
    );
    tree.insert(
        "tests/unit/test_workflows.py",
        b"from myapp.workflows import run_wf\ndef test_wf():\n    assert run_wf() == 1\n".to_vec(),
    );
    tree.insert(
        "tests/integration/test_admin_ui.py",
        br#"
from django.contrib import admin
from django.urls import reverse
def test_admin_page(client):
    assert client is not None
"#
        .to_vec(),
    );
    // Uses `*_test.py` naming convention AND imports `FilesystemResultStorage` directly!
    tree.insert(
        "tests/unit/result_storage_test.py",
        br#"
from myapp.result_storage import FilesystemResultStorage
def test_fs_delete():
    assert FilesystemResultStorage().delete("k") == "k"
"#
        .to_vec(),
    );

    tree
}

#[test]
fn symbol_level_tia_breaks_apps_result_storage_coupling_soundly_and_preserves_cache_duality() {
    let base_tree = build_synthetic_django_repo();

    // Case A: Edit ONLY `FilesystemResultStorage.delete` + add stdlib `import secrets` + helper `_verify_delete`.
    // `validate_storage_configuration` is completely untouched!
    let mut edited_fs_only = base_tree.clone();
    edited_fs_only.insert(
        "src/myapp/result_storage.py",
        br#"
import os
import secrets

_DEFAULT_PREFIX = "results"

class ResultStorageError(RuntimeError):
    pass

def _validate_prefix(cfg):
    return cfg.get("PREFIX", _DEFAULT_PREFIX)

def validate_storage_configuration(cfg):
    if not _validate_prefix(cfg):
        raise ResultStorageError("bad prefix")

def _verify_delete(key):
    return secrets.token_hex(4) + key

class FilesystemResultStorage:
    def delete(self, key):
        return _verify_delete(key)
"#
        .to_vec(),
    );

    let plan_fs = plan_python_tests(
        Some(&base_tree),
        &edited_fs_only,
        None,
        &PlanOptions {
            tier: ConfidenceTier::StrictImpacted,
            ..PlanOptions::default()
        },
    );
    let selected_fs: BTreeSet<&str> = plan_fs.selected_files.iter().map(String::as_str).collect();
    assert!(
        selected_fs.contains("tests/unit/result_storage_test.py"),
        "direct importer of FilesystemResultStorage MUST be selected (and *_test.py discovered!)"
    );
    assert!(
        !selected_fs.contains("tests/integration/test_db_and_cmd.py"),
        "unrelated DB test reaching result_storage only via apps->validate_settings->validate_storage_configuration must be PRUNED: got {selected_fs:?}"
    );
    assert!(plan_fs.impact.symbol_pruned_edges > 0);

    // Check mathematical cache-key duality: `test_db_and_cmd.py` cache key stays IDENTICAL,
    // while `result_storage_test.py` cache key CHANGES!
    let g_base = DependencyGraph::build(&base_tree, None, GraphConfig::default());
    let g_edit = DependencyGraph::build(&edited_fs_only, None, GraphConfig::default());
    let env = BTreeMap::new();
    let k_base = g_base.compute_all_cache_keys(&env, ConfidenceTier::StrictImpacted);
    let k_edit = g_edit.compute_all_cache_keys(&env, ConfidenceTier::StrictImpacted);

    let db_base = k_base
        .iter()
        .find(|k| k.file_path == "tests/integration/test_db_and_cmd.py")
        .unwrap();
    let db_edit = k_edit
        .iter()
        .find(|k| k.file_path == "tests/integration/test_db_and_cmd.py")
        .unwrap();
    assert_eq!(
        db_base.cache_key, db_edit.cache_key,
        "symbol-level cache key for unrelated DB test must remain a cache HIT"
    );

    let rs_base = k_base
        .iter()
        .find(|k| k.file_path == "tests/unit/result_storage_test.py")
        .unwrap();
    let rs_edit = k_edit
        .iter()
        .find(|k| k.file_path == "tests/unit/result_storage_test.py")
        .unwrap();
    assert_ne!(
        rs_base.cache_key, rs_edit.cache_key,
        "cache key for direct FilesystemResultStorage test must invalidate"
    );

    // Case B (Soundness!): Edit `_DEFAULT_PREFIX` (transitively used by `validate_storage_configuration` via `_validate_prefix`!).
    // MUST impact BOTH `test_db_and_cmd.py` AND invalidate its cache key!
    let mut edited_validator_dep = base_tree.clone();
    edited_validator_dep.insert(
        "src/myapp/result_storage.py",
        br#"
import os

_DEFAULT_PREFIX = "results_v2"

class ResultStorageError(RuntimeError):
    pass

def _validate_prefix(cfg):
    return cfg.get("PREFIX", _DEFAULT_PREFIX)

def validate_storage_configuration(cfg):
    if not _validate_prefix(cfg):
        raise ResultStorageError("bad prefix")

class FilesystemResultStorage:
    def delete(self, key):
        return key
"#
        .to_vec(),
    );
    let plan_val = plan_python_tests(
        Some(&base_tree),
        &edited_validator_dep,
        None,
        &PlanOptions {
            tier: ConfidenceTier::StrictImpacted,
            ..PlanOptions::default()
        },
    );
    let selected_val: BTreeSet<&str> = plan_val.selected_files.iter().map(String::as_str).collect();
    assert!(
        selected_val.contains("tests/integration/test_db_and_cmd.py"),
        "editing constant transitively used by validate_storage_configuration MUST impact DB tests!"
    );
    let g_val = DependencyGraph::build(&edited_validator_dep, None, GraphConfig::default());
    let k_val = g_val.compute_all_cache_keys(&env, ConfidenceTier::StrictImpacted);
    let db_val = k_val
        .iter()
        .find(|k| k.file_path == "tests/integration/test_db_and_cmd.py")
        .unwrap();
    assert_ne!(
        db_base.cache_key, db_val.cache_key,
        "editing transitive dependency of validate_storage_configuration MUST invalidate DB cache key!"
    );

    // Case C (`TransitiveImpacted`): symbol pruning is disabled -> editing `FilesystemResultStorage` impacts DB test too!
    let plan_trans = plan_python_tests(
        Some(&base_tree),
        &edited_fs_only,
        None,
        &PlanOptions {
            tier: ConfidenceTier::TransitiveImpacted,
            ..PlanOptions::default()
        },
    );
    let selected_trans: BTreeSet<&str> = plan_trans
        .selected_files
        .iter()
        .map(String::as_str)
        .collect();
    assert!(
        selected_trans.contains("tests/integration/test_db_and_cmd.py"),
        "TransitiveImpacted must disable symbol pruning and run full file-level closure"
    );
}

#[test]
fn transitive_import_reexport_lazy_patch_and_django_soundness() {
    let tree = build_synthetic_django_repo();
    let g = DependencyGraph::build(&tree, None, GraphConfig::default());

    let r = g.compute_impact(
        &["src/myapp/subpkg/helpers.py".to_string()],
        &BTreeSet::new(),
        ConfidenceTier::StrictImpacted,
    );
    let files: BTreeSet<&str> = r.impacted.iter().map(|i| i.test_file.as_str()).collect();
    assert!(files.contains("tests/unit/test_core.py"));
    assert!(files.contains("tests/unit/test_source_check.py"));
    assert!(!files.contains("tests/unit/test_ci_policy.py"));
    assert!(!files.contains("tests/unit/test_workflows.py"));

    for f in ["src/myapp/lazy_dep.py", "src/myapp/patched_target.py"] {
        let res = g.compute_impact(
            &[f.to_string()],
            &BTreeSet::new(),
            ConfidenceTier::StrictImpacted,
        );
        let hit: BTreeSet<&str> = res.impacted.iter().map(|i| i.test_file.as_str()).collect();
        assert!(
            hit.contains("tests/unit/test_lazy_and_patch.py"),
            "missed {f}"
        );
        assert_eq!(hit.len(), 1);
    }

    // Django migrations (existing + brand new), management commands, admin & templates
    for changed in [
        "src/myapp/migrations/0001_initial.py",
        "src/myapp/migrations/0002_new_index.py",
        "src/myapp/management/commands/run_worker.py",
        "src/myapp/shared_defaults.py",
    ] {
        let res = g.compute_impact(
            &[changed.to_string()],
            &BTreeSet::new(),
            ConfidenceTier::StrictImpacted,
        );
        let hit: BTreeSet<&str> = res.impacted.iter().map(|i| i.test_file.as_str()).collect();
        assert!(
            hit.contains("tests/integration/test_db_and_cmd.py"),
            "missed DB test for {changed}"
        );
    }

    for asset in [
        "src/myapp/admin.py",
        "testproject/templates/index.html",
        "testproject/urls.py",
    ] {
        let res = g.compute_impact(
            &[asset.to_string()],
            &BTreeSet::new(),
            ConfidenceTier::StrictImpacted,
        );
        let hit: BTreeSet<&str> = res.impacted.iter().map(|i| i.test_file.as_str()).collect();
        assert!(
            hit.contains("tests/integration/test_admin_ui.py"),
            "missed {asset}"
        );
    }
}

#[test]
fn conftest_scoping_fail_closed_and_source_introspection_guards() {
    let tree = build_synthetic_django_repo();
    let smart_g = DependencyGraph::build(&tree, None, GraphConfig::default());

    let smart_res = smart_g.compute_impact(
        &["src/myapp/workflows.py".to_string()],
        &BTreeSet::new(),
        ConfidenceTier::StrictImpacted,
    );
    assert_eq!(smart_res.impacted_test_files, 1);
    assert_eq!(
        smart_res.impacted[0].test_file,
        "tests/unit/test_workflows.py"
    );

    let (strict_cfg, _) =
        GraphConfig::default().effective_for_tier(ConfidenceTier::TransitiveImpacted);
    let strict_g = DependencyGraph::build(&tree, None, strict_cfg);
    let strict_res = strict_g.compute_impact(
        &["src/myapp/workflows.py".to_string()],
        &BTreeSet::new(),
        ConfidenceTier::TransitiveImpacted,
    );
    assert_eq!(strict_res.impacted_test_files, strict_res.total_test_files);

    // Direct conftest edit, uv.lock edit, or unknown config edit -> FullVerified!
    for trig in ["tests/conftest.py", "uv.lock", "deploy/mystery.cfg"] {
        let res = smart_g.compute_impact(
            &[trig.to_string()],
            &BTreeSet::new(),
            ConfidenceTier::StrictImpacted,
        );
        assert_eq!(
            res.effective_tier,
            ConfidenceTier::FullVerified,
            "failed closed for {trig}"
        );
    }

    // Comment-only edit on `test_source_check.py` ITSELF and on its dependency `helpers.py` must run!
    let mut comment_only = BTreeSet::new();
    comment_only.insert("tests/unit/test_source_check.py".to_string());
    comment_only.insert("tests/unit/test_core.py".to_string());
    let self_res = smart_g.compute_impact(
        &[
            "tests/unit/test_source_check.py".to_string(),
            "tests/unit/test_core.py".to_string(),
        ],
        &comment_only,
        ConfidenceTier::StrictImpacted,
    );
    let self_hit: BTreeSet<&str> = self_res
        .impacted
        .iter()
        .map(|i| i.test_file.as_str())
        .collect();
    assert!(self_hit.contains("tests/unit/test_source_check.py"));
    assert!(!self_hit.contains("tests/unit/test_core.py"));
}

#[test]
fn dynamic_env_proxy_and_allowlisted_env_prefixes_invalidate_cache_keys() {
    let tree = build_synthetic_django_repo();

    // Dynamic trace reports `test_core.py::test_compute` dynamically read `CUSTOM_FEATURE_FLAG`
    let dyn_idx = DynamicTraceIndex::from_trace_file(DynamicTraceFile {
        schema_version: 1,
        python_version: "3.12.12".to_string(),
        monitoring_backend: "sys.monitoring.PY_START+DISABLE(file)+audit+env".to_string(),
        total_wall_ms: 5.0,
        tests: vec![DynamicTestTrace {
            nodeid: "tests/unit/test_core.py::test_compute".to_string(),
            file_path: "tests/unit/test_core.py".to_string(),
            outcome: "passed".to_string(),
            duration_ms: 1.0,
            env_vars_read: BTreeMap::from([(
                "CUSTOM_FEATURE_FLAG".to_string(),
                Some("1".to_string()),
            )]),
            ..Default::default()
        }],
    });

    let g = DependencyGraph::build(&tree, Some(&dyn_idx), GraphConfig::default());

    let env1 = BTreeMap::from([
        ("CUSTOM_FEATURE_FLAG".to_string(), "1".to_string()),
        (
            "DJANGO_SETTINGS_MODULE".to_string(),
            "settings_a".to_string(),
        ),
    ]);
    let env2 = BTreeMap::from([
        ("CUSTOM_FEATURE_FLAG".to_string(), "0".to_string()),
        (
            "DJANGO_SETTINGS_MODULE".to_string(),
            "settings_a".to_string(),
        ),
    ]);
    let env3 = BTreeMap::from([
        ("CUSTOM_FEATURE_FLAG".to_string(), "1".to_string()),
        (
            "DJANGO_SETTINGS_MODULE".to_string(),
            "settings_b".to_string(),
        ),
    ]);

    let k1 = g.compute_all_cache_keys(&env1, ConfidenceTier::StrictImpacted);
    let k2 = g.compute_all_cache_keys(&env2, ConfidenceTier::StrictImpacted);
    let k3 = g.compute_all_cache_keys(&env3, ConfidenceTier::StrictImpacted);

    let core1 = k1
        .iter()
        .find(|k| k.file_path == "tests/unit/test_core.py")
        .unwrap();
    let core2 = k2
        .iter()
        .find(|k| k.file_path == "tests/unit/test_core.py")
        .unwrap();
    let wf1 = k1
        .iter()
        .find(|k| k.file_path == "tests/unit/test_workflows.py")
        .unwrap();
    let wf2 = k2
        .iter()
        .find(|k| k.file_path == "tests/unit/test_workflows.py")
        .unwrap();
    let wf3 = k3
        .iter()
        .find(|k| k.file_path == "tests/unit/test_workflows.py")
        .unwrap();

    // `CUSTOM_FEATURE_FLAG` change invalidates `test_core.py` (which read it) but NOT `test_workflows.py`!
    assert_ne!(core1.cache_key, core2.cache_key);
    assert_eq!(wf1.cache_key, wf2.cache_key);
    assert!(matches!(
        core1.safety,
        CacheSafety::EnvDependentCacheable { .. }
    ));

    // Allowlisted `DJANGO_*` prefix change invalidates ALL tests (incl. `test_workflows.py`)!
    assert_ne!(wf1.cache_key, wf3.cache_key);
}

#[test]
fn policy_reads_authoritative_config_from_base_tree_and_bounds_receipts() {
    let mut base = build_synthetic_django_repo();
    base.insert(
        ".pyrn.toml",
        b"unknown_path_policy = \"full\"\nextra_full_triggers = [\"custom_trigger.cfg\"]\n"
            .to_vec(),
    );

    // Malicious candidate tries to weaken `.pyrn.toml` to `mapped_only` while modifying `custom_trigger.cfg`!
    let mut cand = base.clone();
    cand.insert(
        ".pyrn.toml",
        b"unknown_path_policy = \"mapped_only\"\n".to_vec(),
    );
    cand.insert("custom_trigger.cfg", b"tampered = true\n".to_vec());

    let plan = plan_python_tests(Some(&base), &cand, None, &PlanOptions::default());
    assert_eq!(
        plan.impact.effective_tier,
        ConfidenceTier::FullVerified,
        "authoritative base_tree policy must fail closed even when candidate edits .pyrn.toml"
    );

    // Verify receipt & crash capsule byte ceilings and generic KV metrics prefixing!
    let receipt_bytes = plan.receipt.to_bounded_json();
    assert!(receipt_bytes.len() <= MAX_RECEIPT_BYTES);
    assert!(
        plan.metrics
            .to_kv_line(DEFAULT_METRICS_PREFIX)
            .starts_with("PYRN_METRIC ")
    );
    assert!(
        plan.metrics
            .to_kv_line("CUSTOM_METRIC")
            .starts_with("CUSTOM_METRIC ")
    );

    let long_err = "E   AssertionError: ".to_string() + &"x".repeat(400);
    let cap =
        CrashCapsule::from_failure("pyrn-test", "tests/unit/test_big.py::test_boom", &long_err);
    assert!(cap.summary.len() <= MAX_CRASH_CAPSULE_BYTES);
    assert_eq!(ExecVerdict::Pass.as_str(), "pass");
    assert_eq!(ExecVerdict::Failed.as_str(), "failed");
    assert_eq!(ExecVerdict::Flaky.as_str(), "flaky");
    assert_eq!(ExecVerdict::Pass.exit_code(), 0);
    assert_eq!(ExecVerdict::Flaky.exit_code(), 0);
    assert_eq!(ExecVerdict::Failed.exit_code(), 1);
    assert_eq!(ExecVerdict::ConfigError.exit_code(), 2);
}

#[test]
fn pyrn_toml_and_symbol_diff_handle_preamble_changes_fail_closed() {
    let cfg = PyrnConfig::default();
    let before = parse_python_file("src/m.py", b"X = 1\ndef f():\n    return X\n", "r1", &cfg);
    // Adding top-level executable side-effect call MUST change preamble -> `ChangedSymbols::All`
    let after = parse_python_file(
        "src/m.py",
        b"X = 1\nregister_global()\ndef f():\n    return X\n",
        "r2",
        &cfg,
    );
    assert_eq!(diff_python_symbols(&before, &after), ChangedSymbols::All);

    // Pure `CompactReceipt` schema check
    let r = CompactReceipt {
        schema: CompactReceipt::SCHEMA_V1.to_string(),
        verdict: ExecVerdict::Flaky,
        tier: ConfidenceTier::StrictImpacted,
        role: SlotRole::Verify,
        env_fingerprint: "0123456789abcdef0123456789abcdef".to_string(),
        wall_ms: 42,
        tests_total: 10,
        tests_selected: 2,
        tests_cached: 8,
        tests_tainted: 0,
        tests_passed: 10,
        tests_failed: 0,
        tests_skipped: 0,
        tests_flaky: 1,
        worker_restarts: 0,
        fast_path_tests: 2,
        crash_capsule: None,
        summary: "flaky".to_string(),
    };
    assert!(r.to_bounded_json().len() <= MAX_RECEIPT_BYTES);
}

#[cfg(feature = "cache")]
#[test]
fn cache_refuses_failed_partial_fail_cross_role_and_supports_pruning() {
    use pyrn_core::{CachedTestEntry, CasTestCache};

    let dyn_idx = DynamicTraceIndex::from_trace_file(DynamicTraceFile {
        schema_version: 1,
        python_version: "3.12.12".to_string(),
        monitoring_backend: "sys.monitoring".to_string(),
        total_wall_ms: 5.0,
        tests: vec![
            DynamicTestTrace {
                nodeid: "tests/unit/test_core.py::test_compute[a]".to_string(),
                file_path: "tests/unit/test_core.py".to_string(),
                outcome: "passed".to_string(),
                duration_ms: 1.0,
                ..Default::default()
            },
            DynamicTestTrace {
                nodeid: "tests/unit/test_core.py::test_compute[b]".to_string(),
                file_path: "tests/unit/test_core.py".to_string(),
                outcome: "failed".to_string(),
                duration_ms: 1.0,
                ..Default::default()
            },
        ],
    });
    assert!(
        !dyn_idx
            .by_base_nodeid
            .get("tests/unit/test_core.py::test_compute")
            .unwrap()
            .all_passed()
    );

    let mut cache = CasTestCache::open_in_memory().unwrap();
    cache
        .put_batch(&[
            CachedTestEntry {
                cache_key: "k_pass".to_string(),
                role: SlotRole::Agent,
                project_fp: "p1".to_string(),
                env_fingerprint: "e1".to_string(),
                nodeid: "t1".to_string(),
                outcome: "passed".to_string(),
                duration_ms: 1.0,
                commit_sha: "abc".to_string(),
                witness_128b: String::new(),
                confidence_tier: "strict_impacted".to_string(),
                created_at_epoch_s: 1_000,
                hit_count: 0,
            },
            CachedTestEntry {
                cache_key: "k_fail".to_string(),
                role: SlotRole::Agent,
                project_fp: "p1".to_string(),
                env_fingerprint: "e1".to_string(),
                nodeid: "t2".to_string(),
                outcome: "failed".to_string(),
                duration_ms: 1.0,
                commit_sha: "abc".to_string(),
                witness_128b: "boom".to_string(),
                confidence_tier: "strict_impacted".to_string(),
                created_at_epoch_s: 1_000,
                hit_count: 0,
            },
        ])
        .unwrap();

    let hits_agent = cache
        .lookup_passing_hits(
            SlotRole::Agent,
            "p1",
            "e1",
            &["k_pass".to_string(), "k_fail".to_string()],
            true,
        )
        .unwrap();
    assert!(hits_agent.contains_key("k_pass"));
    assert!(!hits_agent.contains_key("k_fail"));

    // Cross-role isolation (`Verify` sees 0 `Agent` entries)
    let hits_verify = cache
        .lookup_passing_hits(SlotRole::Verify, "p1", "e1", &["k_pass".to_string()], false)
        .unwrap();
    assert!(hits_verify.is_empty());

    // Prune old entries (`created_at_epoch_s = 1_000` is older than 1 day!)
    let pruned = cache
        .prune_older_than_days(1, Some(SlotRole::Agent))
        .unwrap();
    assert_eq!(pruned, 2);
    assert_eq!(cache.stats().unwrap().total_entries, 0);
}

#[test]
fn package_init_reexport_and_dunder_main_soundness_and_cache_duality() {
    let mut base = InMemoryTree::default();
    base.insert("pyproject.toml", b"[project]\nname = \"demo\"\n".to_vec());
    base.insert(
        "src/pkg/__init__.py",
        b"from pkg.entry import execute_task\n__all__ = [\"execute_task\"]\n".to_vec(),
    );
    base.insert(
        "src/pkg/storage.py",
        b"class FSStorage:\n    def delete(self):\n        return 1\n".to_vec(),
    );
    base.insert(
        "src/pkg/entry.py",
        br#"
def execute_task():
    from pkg.storage import FSStorage
    return FSStorage().delete()

def main():
    return execute_task()

if __name__ == "__main__":
    raise SystemExit(main())
"#
        .to_vec(),
    );
    base.insert(
        "src/pkg/sibling.py",
        b"def sibling_helper():\n    return 42\n".to_vec(),
    );
    base.insert(
        "tests/test_sibling.py",
        b"from pkg.sibling import sibling_helper\ndef test_sib():\n    assert sibling_helper() == 42\n"
            .to_vec(),
    );
    base.insert(
        "tests/test_entry.py",
        b"from pkg import execute_task\ndef test_ent():\n    assert execute_task() == 1\n".to_vec(),
    );

    // 1. Editing `FSStorage.delete` must impact `test_entry.py`, MUST NOT impact `test_sibling.py`
    //    (even though `test_sibling.py` triggers `pkg/__init__.py` via PackageInit and `entry.py` has
    //    `if __name__ == "__main__": main()`), AND must preserve `test_sibling.py`'s cache key!
    let mut edit_storage = base.clone();
    edit_storage.insert(
        "src/pkg/storage.py",
        b"class FSStorage:\n    def delete(self):\n        return 2\n".to_vec(),
    );
    let plan1 = plan_python_tests(Some(&base), &edit_storage, None, &PlanOptions::default());
    let impacted1: BTreeSet<&str> = plan1
        .impact
        .impacted
        .iter()
        .map(|i| i.test_file.as_str())
        .collect();
    assert!(impacted1.contains("tests/test_entry.py"));
    assert!(
        !impacted1.contains("tests/test_sibling.py"),
        "PackageInit re-export must NOT fan out to unrelated sibling: {impacted1:?}"
    );

    let g_base = DependencyGraph::build(
        &base,
        None,
        GraphConfig::from_project(PyrnConfig::from_tree(&base)),
    );
    let g_edit = DependencyGraph::build(
        &edit_storage,
        None,
        GraphConfig::from_project(PyrnConfig::from_tree(&edit_storage)),
    );
    let k_base: BTreeMap<String, String> = g_base
        .compute_all_cache_keys(&BTreeMap::new(), ConfidenceTier::StrictImpacted)
        .into_iter()
        .map(|r| (r.nodeid, r.cache_key))
        .collect();
    let k_edit: BTreeMap<String, String> = g_edit
        .compute_all_cache_keys(&BTreeMap::new(), ConfidenceTier::StrictImpacted)
        .into_iter()
        .map(|r| (r.nodeid, r.cache_key))
        .collect();
    assert_eq!(
        k_base["tests/test_sibling.py::test_sib"], k_edit["tests/test_sibling.py::test_sib"],
        "cache key for sibling test must remain stable across unrelated FSStorage edit"
    );
    assert_ne!(
        k_base["tests/test_entry.py::test_ent"], k_edit["tests/test_entry.py::test_ent"],
        "cache key for entry test MUST invalidate when FSStorage changes"
    );

    // 2. Direct edit to `pkg/__init__.py` MUST impact `test_sibling.py` AND invalidate its cache key!
    let mut edit_init = base.clone();
    edit_init.insert(
        "src/pkg/__init__.py",
        b"from pkg.entry import execute_task\nINIT_FLAG = 1\n__all__ = [\"execute_task\"]\n"
            .to_vec(),
    );
    let plan2 = plan_python_tests(Some(&base), &edit_init, None, &PlanOptions::default());
    assert!(
        plan2
            .impact
            .impacted
            .iter()
            .any(|i| i.test_file == "tests/test_sibling.py"),
        "direct edit to pkg/__init__.py must impact sibling tests"
    );
    let g_init = DependencyGraph::build(
        &edit_init,
        None,
        GraphConfig::from_project(PyrnConfig::from_tree(&edit_init)),
    );
    let k_init: BTreeMap<String, String> = g_init
        .compute_all_cache_keys(&BTreeMap::new(), ConfidenceTier::StrictImpacted)
        .into_iter()
        .map(|r| (r.nodeid, r.cache_key))
        .collect();
    assert_ne!(
        k_base["tests/test_sibling.py::test_sib"], k_init["tests/test_sibling.py::test_sib"],
        "direct edit to pkg/__init__.py must invalidate sibling cache key"
    );

    // 3. If `pkg/__init__.py` calls `execute_task()` at import time (preamble), editing `FSStorage`
    //    MUST impact `test_sibling.py` AND invalidate its cache key (zero false negatives)!
    let mut base_eager = base.clone();
    base_eager.insert(
        "src/pkg/__init__.py",
        b"from pkg.entry import execute_task\nBOOT = execute_task()\n__all__ = [\"execute_task\"]\n"
            .to_vec(),
    );
    let mut edit_eager = base_eager.clone();
    edit_eager.insert(
        "src/pkg/storage.py",
        b"class FSStorage:\n    def delete(self):\n        return 2\n".to_vec(),
    );
    let plan3 = plan_python_tests(
        Some(&base_eager),
        &edit_eager,
        None,
        &PlanOptions::default(),
    );
    assert!(
        plan3
            .impact
            .impacted
            .iter()
            .any(|i| i.test_file == "tests/test_sibling.py"),
        "import-time call in pkg/__init__.py must fail closed to all sibling tests"
    );
    let g_eb = DependencyGraph::build(
        &base_eager,
        None,
        GraphConfig::from_project(PyrnConfig::from_tree(&base_eager)),
    );
    let g_ee = DependencyGraph::build(
        &edit_eager,
        None,
        GraphConfig::from_project(PyrnConfig::from_tree(&edit_eager)),
    );
    let k_eb: BTreeMap<String, String> = g_eb
        .compute_all_cache_keys(&BTreeMap::new(), ConfidenceTier::StrictImpacted)
        .into_iter()
        .map(|r| (r.nodeid, r.cache_key))
        .collect();
    let k_ee: BTreeMap<String, String> = g_ee
        .compute_all_cache_keys(&BTreeMap::new(), ConfidenceTier::StrictImpacted)
        .into_iter()
        .map(|r| (r.nodeid, r.cache_key))
        .collect();
    assert_ne!(
        k_eb["tests/test_sibling.py::test_sib"], k_ee["tests/test_sibling.py::test_sib"],
        "import-time call in pkg/__init__.py must invalidate sibling cache key"
    );

    // 4. Deleting `execute_task` from `entry.py` breaks `pkg/__init__.py` at import time (`ImportError`)
    //    -> MUST fail closed and impact `test_sibling.py`!
    let mut del_sym = base.clone();
    del_sym.insert("src/pkg/entry.py", b"def other():\n    return 0\n".to_vec());
    let plan4 = plan_python_tests(Some(&base), &del_sym, None, &PlanOptions::default());
    assert!(
        plan4
            .impact
            .impacted
            .iter()
            .any(|i| i.test_file == "tests/test_sibling.py"),
        "deleting a re-exported symbol must fail closed to catch ImportError in sibling tests"
    );
}

#[test]
fn multi_file_unmapped_no_cross_bleed_and_deleted_global_trigger_fail_closed() {
    // 1. Multi-file commit: `src/pkg/mod_a.py` prunes an unrelated symbol edge (so `file_symbol_pruned > 0`),
    //    AND `src/pkg/orphan_plugin.py` has ZERO static importers (`matched_any == false`, `file_symbol_pruned == 0`).
    //    Under `UnknownPathPolicy::Full`, `mod_a.py`'s pruned edge MUST NOT bleed over and exempt `orphan_plugin.py`!
    let mut base = InMemoryTree::default();
    base.insert("pyproject.toml", b"[project]\nname = \"demo\"\n".to_vec());
    base.insert("src/pkg/__init__.py", b"".to_vec());
    base.insert(
        "src/pkg/mod_a.py",
        b"def used_fn():\n    return 1\ndef unused_fn():\n    return 2\n".to_vec(),
    );
    base.insert(
        "src/pkg/orphan_plugin.py",
        b"def plugin_hook():\n    return 'v1'\n".to_vec(),
    );
    base.insert(
        "tests/test_a.py",
        b"from pkg.mod_a import used_fn\ndef test_used():\n    assert used_fn() == 1\n".to_vec(),
    );

    let mut cand = base.clone();
    cand.insert(
        "src/pkg/mod_a.py",
        b"def used_fn():\n    return 1\ndef unused_fn():\n    return 999\n".to_vec(),
    );
    cand.insert(
        "src/pkg/orphan_plugin.py",
        b"def plugin_hook():\n    return 'v2'\n".to_vec(),
    );
    let plan = plan_python_tests(Some(&base), &cand, None, &PlanOptions::default());
    assert_eq!(
        plan.impact.effective_tier,
        ConfidenceTier::FullVerified,
        "pruned edge on mod_a.py must NOT suppress UnknownPathPolicy::Full fallback for orphan_plugin.py"
    );
    assert!(
        plan.impact
            .full_fallback_reasons
            .iter()
            .any(|r| r.contains("orphan_plugin.py")),
        "expected full_fallback_reasons to cite orphan_plugin.py, got {:?}",
        plan.impact.full_fallback_reasons
    );

    // 2. Deleting a global trigger (`.pyrn.toml` / `pytest.ini`) even under `UnknownPathPolicy::MappedOnly`
    //    MUST still trigger `ConfidenceTier::FullVerified` (global triggers are registered unconditionally).
    let mut base_cfg = base.clone();
    base_cfg.insert(
        ".pyrn.toml",
        b"unknown_path_policy = \"mapped_only\"\n".to_vec(),
    );
    let mut cand_del_cfg = base.clone();
    // `.pyrn.toml` deleted in candidate (not present in candidate tree at all!)
    cand_del_cfg.insert("pytest.ini", b"[pytest]\n".to_vec());
    let plan_del = plan_python_tests(
        Some(&base_cfg),
        &cand_del_cfg,
        None,
        &PlanOptions::default(),
    );
    assert_eq!(
        plan_del.impact.effective_tier,
        ConfidenceTier::FullVerified,
        "deleting `.pyrn.toml` or adding `pytest.ini` must trigger FullVerified even under mapped_only"
    );
    assert!(
        plan_del
            .impact
            .full_fallback_reasons
            .iter()
            .any(|r| r.contains(".pyrn.toml")),
        "expected deleted `.pyrn.toml` to hit global trigger, got {:?}",
        plan_del.impact.full_fallback_reasons
    );
}

#[test]
fn inherited_unittest_zero_static_test_file_is_selected_by_tia() {
    let mut base = InMemoryTree::default();
    base.insert("pyproject.toml", b"[project]\nname = \"demo\"\n".to_vec());
    base.insert(
        "tests/base_helpers.py",
        b"import unittest\nclass BaseCheck(unittest.TestCase):\n    def test_inherited(self):\n        assert True\n"
            .to_vec(),
    );
    base.insert(
        "tests/test_child.py",
        b"from tests.base_helpers import BaseCheck\nclass TestChild(BaseCheck):\n    pass\n"
            .to_vec(),
    );

    // 1. Editing the base class file MUST select `tests/test_child.py` even though it has 0 static `def test_*`!
    let mut edit_base = base.clone();
    edit_base.insert(
        "tests/base_helpers.py",
        b"import unittest\nclass BaseCheck(unittest.TestCase):\n    def test_inherited(self):\n        assert False\n"
            .to_vec(),
    );
    let plan1 = plan_python_tests(Some(&base), &edit_base, None, &PlanOptions::default());
    assert!(
        plan1
            .impact
            .impacted
            .iter()
            .any(|i| i.test_file == "tests/test_child.py"),
        "inherited unittest test file with 0 static test funcs must be selected when base class changes: {:?}",
        plan1.impact.impacted
    );

    // 2. Directly editing `tests/test_child.py` MUST also select it!
    let mut edit_child = base.clone();
    edit_child.insert(
        "tests/test_child.py",
        b"from tests.base_helpers import BaseCheck\nclass TestChild(BaseCheck):\n    flag = 1\n"
            .to_vec(),
    );
    let plan2 = plan_python_tests(Some(&base), &edit_child, None, &PlanOptions::default());
    assert!(
        plan2
            .impact
            .impacted
            .iter()
            .any(|i| i.test_file == "tests/test_child.py"),
        "direct edit to inherited unittest test file must select it: {:?}",
        plan2.impact.impacted
    );
}

#[test]
fn symbol_level_tia_breaks_hub_module_fanout_with_full_recall() {
    let mut base = InMemoryTree::default();
    base.insert(
        "pyproject.toml",
        b"[project]\nname = \"hub-app\"\n[tool.hatch.build.targets.wheel]\npackages = [\"src/hubapp\"]\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n".to_vec(),
    );
    base.insert("src/hubapp/__init__.py", b"".to_vec());
    base.insert(
        "src/hubapp/storage.py",
        b"class LocalStorage:\n    def delete(self, k):\n        return f'rm:{k}'\n\nclass RemoteStorage:\n    def fetch(self, k):\n        return f'get:{k}'\n".to_vec(),
    );
    base.insert(
        "src/hubapp/settings.py",
        b"from hubapp.storage import RemoteStorage\nDEFAULT_REMOTE = RemoteStorage\n".to_vec(),
    );
    base.insert(
        "tests/test_local_storage.py",
        b"from hubapp.storage import LocalStorage\ndef test_delete():\n    assert LocalStorage().delete('a') == 'rm:a'\n".to_vec(),
    );
    base.insert(
        "tests/test_remote_storage.py",
        b"from hubapp.storage import RemoteStorage\ndef test_fetch():\n    assert RemoteStorage().fetch('a') == 'get:a'\n".to_vec(),
    );
    base.insert(
        "tests/test_settings.py",
        b"from hubapp.settings import DEFAULT_REMOTE\ndef test_default():\n    assert DEFAULT_REMOTE is not None\n".to_vec(),
    );

    // Edit ONLY `LocalStorage.delete`: symbol-level TIA must select `test_local_storage.py`
    // while pruning `test_remote_storage.py` and `test_settings.py`!
    let mut cand = base.clone();
    cand.insert(
        "src/hubapp/storage.py",
        b"class LocalStorage:\n    def delete(self, k):\n        return f'unlink:{k}'\n\nclass RemoteStorage:\n    def fetch(self, k):\n        return f'get:{k}'\n".to_vec(),
    );

    let sym_plan = plan_python_tests(Some(&base), &cand, None, &PlanOptions::default());
    assert_eq!(
        sym_plan.selected_files,
        vec!["tests/test_local_storage.py".to_string()],
        "symbol-level TIA should prune unrelated hub importers"
    );
    assert!(
        sym_plan.impact.symbol_pruned_edges >= 2,
        "expected >= 2 symbol-pruned edges, got {}",
        sym_plan.impact.symbol_pruned_edges
    );

    // Compare against transitive file-level tier (must conservatively select all 3 files)
    let file_opts = PlanOptions {
        tier: ConfidenceTier::TransitiveImpacted,
        ..PlanOptions::default()
    };
    let file_plan = plan_python_tests(Some(&base), &cand, None, &file_opts);
    assert_eq!(file_plan.selected_files.len(), 3);
}

#[cfg(all(feature = "fs", feature = "process"))]
#[test]
fn optional_bench_repo_git_replay_preserves_cochange_recall() {
    use std::path::PathBuf;

    let Ok(repo_str) = std::env::var("PYRN_BENCH_REPO") else {
        return;
    };
    let repo = PathBuf::from(repo_str);
    if !repo.is_dir() {
        return;
    }

    let tree = pyrn_core::FsTreeSource::load_from_fs(&repo).unwrap();
    let cfg = GraphConfig::from_project(PyrnConfig::from_tree(&tree));
    let mut graph = DependencyGraph::build(&tree, None, cfg);

    if let Ok(csv_str) = std::env::var("PYRN_BENCH_TIMINGS_CSV")
        && let Ok(raw) = std::fs::read_to_string(&csv_str)
    {
        for line in raw.lines().skip(1) {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() >= 3
                && let Ok(sec) = parts[2].trim().parse::<f64>()
            {
                graph
                    .file_durations_ms
                    .insert(parts[0].trim().replace('\\', "/"), sec * 1000.0);
            }
        }
    }

    let summary =
        pyrn_core::replay_git_history(&repo, &graph, 30, ConfidenceTier::StrictImpacted).unwrap();

    assert_eq!(
        summary.total_cochange_predicted_tests,
        summary.total_cochange_existing_tests,
        "symbol-level TIA must maintain 100% co-change recall! missed: {:?}",
        summary
            .rows
            .iter()
            .filter(|r| !r.cochange_missed_tests.is_empty())
            .map(|r| (&r.sha, &r.cochange_missed_tests))
            .collect::<Vec<_>>()
    );
}
