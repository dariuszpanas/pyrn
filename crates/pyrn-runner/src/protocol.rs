//! Wire protocol types for Orchestrator <-> Worker and Client <-> Warm Daemon.
//!
//! Shared execution contracts (`SlotRole`, `ConfidenceTier`, `ResourceLane`, `TestOutcome`,
//! `ExecVerdict`, `CrashCapsule`, `CompactReceipt`, `StreamEvent`, `RunMetrics`) are imported
//! directly from `pyrn_core::receipt` so `pyrn-core`, `pyrn-runner`, and `pyrn-cli` speak one
//! unified contract.

pub use pyrn_core::dynamic::{DynamicTestTrace, DynamicTraceFile};
pub use pyrn_core::receipt::{
    CompactReceipt, ConfidenceTier, CrashCapsule, DEFAULT_METRICS_PREFIX, ExecVerdict,
    MAX_CRASH_CAPSULE_BYTES, MAX_RECEIPT_BYTES, ResourceLane, RunMetrics, SlotRole, StreamEvent,
    TestOutcome,
};
use serde::{Deserialize, Serialize};

/// Commands sent from Rust supervisor -> Python worker over loopback NDJSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum WorkerCommand {
    RunUnit {
        unit_id: String,
        paths: Vec<String>,
        markers: Option<String>,
        keyword: Option<String>,
        shard_index: usize,
        shard_total: usize,
        explicit_nodeids: Option<Box<Vec<String>>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exclude_nodeids: Option<Box<Vec<String>>>,
        fast_path: bool,
        changed_paths: Vec<String>,
        #[serde(default)]
        trace: bool,
        #[serde(default = "default_trace_granularity")]
        trace_granularity: String,
        #[serde(default)]
        trace_symbols: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cov_data_file: Option<String>,
    },
    Invalidate {
        changed_paths: Vec<String>,
    },
    Ping,
    Shutdown,
}

fn default_trace_granularity() -> String {
    "file".to_owned()
}

/// Events streamed from Python worker -> Rust supervisor over loopback NDJSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorkerEvent {
    Hello {
        auth_token: String,
        worker_id: String,
        pid: u32,
        lane: ResourceLane,
        boot_ms: f64,
    },
    Pong {
        worker_id: String,
    },
    Invalidated {
        worker_id: String,
    },
    UnitCollected {
        unit_id: String,
        nodeids: Vec<String>,
        deselected: usize,
        collect_ms: f64,
    },
    UnitCollectError {
        unit_id: String,
        collect_ms: f64,
        error: String,
    },
    TestStart {
        unit_id: String,
        nodeid: String,
    },
    TestResult {
        unit_id: String,
        nodeid: String,
        outcome: TestOutcome,
        setup_ms: f64,
        call_ms: f64,
        teardown_ms: f64,
        fast_path: bool,
        longrepr: Option<String>,
        #[serde(default)]
        trace: Option<Box<DynamicTestTrace>>,
    },
    UnitDone {
        unit_id: String,
        db_setup_ms: f64,
        db_template_hit: bool,
        #[serde(default)]
        db_template_hits: u32,
    },
    WorkerError {
        message: String,
    },
}

/// Per-test completed record retained by the Rust orchestrator.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestRecord {
    pub nodeid: String,
    pub outcome: TestOutcome,
    pub setup_ms: f64,
    pub call_ms: f64,
    pub teardown_ms: f64,
    pub total_ms: f64,
    pub worker_id: String,
    pub lane: ResourceLane,
    pub fast_path: bool,
    pub attempts: u32,
    pub longrepr: Option<String>,
}

/// Per-worker utilization & straggler stats.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkerStats {
    pub worker_id: String,
    pub lane: String,
    pub boot_ms: f64,
    pub collect_ms: f64,
    pub db_setup_ms: f64,
    pub db_template_hits: u32,
    pub exec_ms: f64,
    pub wall_active_ms: f64,
    pub tests_run: usize,
    pub fast_path_tests: usize,
    pub units_run: usize,
    pub restarts: u32,
}

/// Complete run report returned by cold execution, warm daemon execution, and unified TIA+CAS runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunReport {
    pub mode: String,
    pub role: SlotRole,
    pub tier: ConfidenceTier,
    pub workers_requested: usize,
    pub worker_limit_reason: String,
    pub wall_ms: u64,
    pub discovery_ms: u64,
    pub pool_ready_ms: u64,
    pub tia_parse_ms: u64,
    pub tia_graph_ms: u64,
    pub collect_total_ms: f64,
    pub db_setup_total_ms: f64,
    pub db_template_hits: u32,
    pub total_collected: usize,
    pub tests_selected: usize,
    pub tests_cached: usize,
    pub tests_tainted: usize,
    pub deselected: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub xfailed: usize,
    pub xpassed: usize,
    pub errors: usize,
    pub timed_out: usize,
    pub crashed: usize,
    pub flaky: usize,
    pub fast_path_tests: usize,
    pub full_pytest_tests: usize,
    pub worker_restarts: u32,
    pub coverage_file: Option<String>,
    pub collection_errors: Vec<String>,
    pub verdict: ExecVerdict,
    pub exit_code: i32,
    pub worker_stats: Vec<WorkerStats>,
    pub results: Vec<TestRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traces: Vec<DynamicTestTrace>,
}

/// Client -> Warm Daemon RPC requests.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequest {
    Status {
        auth_token: String,
    },
    Stop {
        auth_token: String,
    },
    Run {
        auth_token: String,
        paths: Vec<String>,
        markers: Option<String>,
        keyword: Option<String>,
        test_timeout_secs: u64,
        reruns: u32,
        fast_path: bool,
        shard_heavy: bool,
        #[serde(default)]
        trace: bool,
        #[serde(default = "default_trace_granularity")]
        trace_granularity: String,
        #[serde(default)]
        trace_symbols: bool,
        #[serde(default)]
        cov_sysmon: bool,
        #[serde(default)]
        cov_out: Option<String>,
        #[serde(default)]
        explicit_nodeids_by_file: Option<Box<std::collections::BTreeMap<String, Vec<String>>>>,
        #[serde(default)]
        exclude_nodeids_by_file: Option<Box<std::collections::BTreeMap<String, Vec<String>>>>,
    },
}

/// Warm Daemon -> Client RPC responses.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonResponse {
    Status {
        pid: u32,
        project_root: String,
        role: SlotRole,
        workers_total: usize,
        workers_hermetic: usize,
        workers_sqlite: usize,
        code_epoch: String,
        migrations_hash: String,
        runs_served: u64,
    },
    Stopped,
    RunResult {
        report: Box<RunReport>,
    },
    Error {
        message: String,
    },
}
