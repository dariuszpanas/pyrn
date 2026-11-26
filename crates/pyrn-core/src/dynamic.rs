//! Dynamic execution trace model produced by the Python 3.12+ PEP 669 `sys.monitoring`
//! + PEP 578 `sys.addaudithook` + `os.environ` read-proxy tracer and consumed by `pyrn-core`.
//!
//! Pure in-memory JSON parsing in the default build; disk loading gated behind `feature = "fs"`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::tree::normalize_rel_path;

/// One test node's dynamically observed runtime dependencies and execution metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DynamicTestTrace {
    /// Pytest node ID, e.g. `tests/unit/test_a.py::TestCls::test_foo[case1]`.
    pub nodeid: String,
    /// Repository-relative test file path.
    pub file_path: String,
    /// Outcome (`passed`, `failed`, `skipped`, `xfailed`, `error`).
    pub outcome: String,
    /// Wall-clock duration in milliseconds (setup + call + teardown).
    pub duration_ms: f64,
    /// Repository-relative `.py` files whose bytecode executed during this test (`PY_START`).
    #[serde(default)]
    pub executed_py_files: Vec<String>,
    /// Executed `rel_path::qualname` symbols captured by `PY_START`.
    #[serde(default)]
    pub executed_symbols: Vec<String>,
    /// Repository-relative non-Python assets opened/read during this test.
    #[serde(default)]
    pub read_assets: Vec<String>,
    /// Repository-relative directories listed/globbed during this test (`os.listdir`/`os.scandir`).
    #[serde(default)]
    pub listed_dirs: Vec<String>,
    /// Environment variable names actually read during this test (captured via `os.environ` read proxy
    /// and `os.getenv`), mapped to observed value presence (`Some(val)` or `None` if unset).
    #[serde(default)]
    pub env_vars_read: BTreeMap<String, Option<String>>,
    /// Pytest fixtures requested/activated for this test item.
    #[serde(default)]
    pub fixtures: Vec<String>,
    /// Pytest markers attached to this test item.
    #[serde(default)]
    pub markers: Vec<String>,
    /// Dynamic taint reasons observed at runtime (`dynamic:subprocess.Popen`, `dynamic:socket.connect`, etc.).
    #[serde(default)]
    pub taint_reasons: Vec<String>,
}

/// Top-level JSON trace bundle emitted by `pyrn_worker.py` or `pytest_pyrn_trace.py`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DynamicTraceFile {
    pub schema_version: u32,
    pub python_version: String,
    pub monitoring_backend: String,
    pub total_wall_ms: f64,
    pub tests: Vec<DynamicTestTrace>,
}

/// Aggregated dynamic trace index keyed by exact nodeid, base nodeid, and test file path.
#[derive(Debug, Clone, Default)]
pub struct DynamicTraceIndex {
    /// Exact per-nodeid trace (including parametrized variants).
    pub by_nodeid: BTreeMap<String, DynamicTestTrace>,
    /// Aggregated per base nodeid (stripping `[param]` suffix for static AST matching).
    pub by_base_nodeid: BTreeMap<String, AggregatedDynamicDeps>,
    /// Aggregated per test file path.
    pub by_test_file: BTreeMap<String, AggregatedDynamicDeps>,
    /// Total observed durations per test file (ms).
    pub file_durations_ms: BTreeMap<String, f64>,
    /// Total trace records loaded.
    pub trace_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct AggregatedDynamicDeps {
    pub executed_py_files: BTreeSet<String>,
    pub executed_symbols: BTreeSet<String>,
    pub read_assets: BTreeSet<String>,
    pub listed_dirs: BTreeSet<String>,
    /// Observed environment variable keys read dynamically (with last observed `Option<String>` value).
    pub env_vars_read: BTreeMap<String, Option<String>>,
    pub taint_reasons: BTreeSet<String>,
    pub total_duration_ms: f64,
    pub dynamic_test_count: usize,
    pub passed_count: usize,
    pub failed_count: usize,
    pub skipped_count: usize,
    pub other_outcome_count: usize,
}

impl AggregatedDynamicDeps {
    /// True ONLY when at least one variant executed and EVERY observed variant passed.
    /// Guarantees failed, errored, skipped, or partially-failing parametrized families are NEVER cached.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.passed_count > 0
            && self.failed_count == 0
            && self.skipped_count == 0
            && self.other_outcome_count == 0
    }
}

impl DynamicTraceIndex {
    /// Parse a JSON trace string purely in memory (works in default no-FS build).
    pub fn from_json_str(raw: &str) -> anyhow::Result<Self> {
        let trace_file: DynamicTraceFile = serde_json::from_str(raw)?;
        Ok(Self::from_trace_file(trace_file))
    }

    #[cfg(feature = "fs")]
    pub fn load_json(path: &std::path::Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read dynamic trace {}", path.display()))?;
        Self::from_json_str(&raw)
            .with_context(|| format!("failed to parse dynamic trace JSON {}", path.display()))
    }

    #[must_use]
    pub fn from_trace_file(trace_file: DynamicTraceFile) -> Self {
        let mut idx = Self::default();
        idx.merge(trace_file);
        idx
    }

    pub fn merge(&mut self, trace_file: DynamicTraceFile) {
        for mut t in trace_file.tests {
            t.file_path = normalize_rel_path(&t.file_path);
            let base = base_nodeid(&t.nodeid);

            let file_agg = self.by_test_file.entry(t.file_path.clone()).or_default();
            let base_agg = self.by_base_nodeid.entry(base).or_default();

            for py in &t.executed_py_files {
                let norm = normalize_rel_path(py);
                file_agg.executed_py_files.insert(norm.clone());
                base_agg.executed_py_files.insert(norm);
            }
            for sym in &t.executed_symbols {
                file_agg.executed_symbols.insert(sym.clone());
                base_agg.executed_symbols.insert(sym.clone());
            }
            for asset in &t.read_assets {
                let norm = normalize_rel_path(asset);
                file_agg.read_assets.insert(norm.clone());
                base_agg.read_assets.insert(norm);
            }
            for dir in &t.listed_dirs {
                let norm = normalize_rel_path(dir).trim_end_matches('/').to_string();
                file_agg.listed_dirs.insert(norm.clone());
                base_agg.listed_dirs.insert(norm);
            }
            for (env_k, env_v) in &t.env_vars_read {
                file_agg.env_vars_read.insert(env_k.clone(), env_v.clone());
                base_agg.env_vars_read.insert(env_k.clone(), env_v.clone());
            }
            for reason in &t.taint_reasons {
                file_agg.taint_reasons.insert(reason.clone());
                base_agg.taint_reasons.insert(reason.clone());
            }
            file_agg.total_duration_ms += t.duration_ms;
            file_agg.dynamic_test_count += 1;
            base_agg.total_duration_ms += t.duration_ms;
            base_agg.dynamic_test_count += 1;

            match t.outcome.as_str() {
                "passed" => {
                    file_agg.passed_count += 1;
                    base_agg.passed_count += 1;
                }
                "failed" | "error" | "timed_out" | "crashed" => {
                    file_agg.failed_count += 1;
                    base_agg.failed_count += 1;
                }
                "skipped" => {
                    file_agg.skipped_count += 1;
                    base_agg.skipped_count += 1;
                }
                _ => {
                    file_agg.other_outcome_count += 1;
                    base_agg.other_outcome_count += 1;
                }
            }

            *self
                .file_durations_ms
                .entry(t.file_path.clone())
                .or_insert(0.0) += t.duration_ms;
            self.by_nodeid.insert(t.nodeid.clone(), t);
            self.trace_count += 1;
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.trace_count == 0
    }
}

/// Strip pytest parametrization brackets from a nodeid:
/// `tests/unit/test_a.py::TestCls::test_foo[bar-1]` -> `tests/unit/test_a.py::TestCls::test_foo`
#[must_use]
pub fn base_nodeid(nodeid: &str) -> String {
    match nodeid.find('[') {
        Some(idx) => nodeid[..idx].to_string(),
        None => nodeid.to_string(),
    }
}
