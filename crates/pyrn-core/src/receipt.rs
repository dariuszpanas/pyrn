//! Shared `pyrn` execution and verification contracts: isolation roles, confidence tiers,
//! verdicts, key-value `RunMetrics` formatting, NDJSON streaming events, `<= 128 B`
//! `CrashCapsule`, and `<= 2,048 B` `CompactReceipt`.

use serde::{Deserialize, Serialize};

/// Hard ceiling for compact verification receipt JSON (`2,048` bytes).
pub const MAX_RECEIPT_BYTES: usize = 2048;

/// Hard ceiling for inline shrunk failure witness (`128` bytes).
pub const MAX_CRASH_CAPSULE_BYTES: usize = 128;

/// Default prefix for key-value metrics output (`--metrics-kv`).
pub const DEFAULT_METRICS_PREFIX: &str = "PYRN_METRIC";

/// Slot/cache isolation role separating interactive development (`agent`) from clean CI (`verify`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotRole {
    #[default]
    Agent,
    Verify,
}

impl SlotRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Verify => "verify",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "agent" => Ok(Self::Agent),
            "verify" => Ok(Self::Verify),
            other => anyhow::bail!("invalid slot role '{other}' (expected 'agent' or 'verify')"),
        }
    }
}

/// Confidence tier controlling TIA precision and fail-closed fallback behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceTier {
    /// Fast inner-loop tier: AST-normalized hashing (auto-upgraded to raw for source-introspecting
    /// tests), symbol-level refinement, smart fixture-scoped conftest resolution, and static+dynamic edges.
    #[default]
    StrictImpacted,
    /// Conservative pre-merge tier: raw file hashing everywhere, `StrictAllImports` conftest closure,
    /// and file-level transitive closure (no symbol filtering).
    TransitiveImpacted,
    /// Full-suite fail-closed verification (automatically triggered on global config/lockfile/conftest
    /// edits or unmapped paths under `UnknownPathPolicy::Full`).
    FullVerified,
}

impl ConfidenceTier {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StrictImpacted => "strict_impacted",
            Self::TransitiveImpacted => "transitive_impacted",
            Self::FullVerified => "full_verified",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "strict" | "strict_impacted" => Ok(Self::StrictImpacted),
            "transitive" | "transitive_impacted" => Ok(Self::TransitiveImpacted),
            "full" | "full_verified" => Ok(Self::FullVerified),
            other => anyhow::bail!(
                "invalid confidence tier '{other}' (expected 'strict', 'transitive', or 'full')"
            ),
        }
    }
}

/// Resource affinity lane for test routing and worker specialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLane {
    /// Pure Python / mocked boundary tests (no DB fixtures, no external services).
    Hermetic,
    /// Django ORM / SQLite database tests.
    SqliteDjango,
    /// External/serial tests (local Ray, live cluster, PostgreSQL).
    SerialExternal,
}

impl ResourceLane {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hermetic => "hermetic",
            Self::SqliteDjango => "sqlite_django",
            Self::SerialExternal => "serial_external",
        }
    }
}

/// Outcome of a single test case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Passed,
    Failed,
    Skipped,
    Xfailed,
    Xpassed,
    Error,
    TimedOut,
    Crashed,
    Flaky,
    Cached,
}

impl TestOutcome {
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::Error | Self::TimedOut | Self::Crashed
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Xfailed => "xfailed",
            Self::Xpassed => "xpassed",
            Self::Error => "error",
            Self::TimedOut => "timed_out",
            Self::Crashed => "crashed",
            Self::Flaky => "flaky",
            Self::Cached => "cached",
        }
    }
}

/// Structured execution verdict for a test run or plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecVerdict {
    Pass,
    Failed,
    TimedOut,
    Flaky,
    ConfigError,
}

impl ExecVerdict {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Flaky => "flaky",
            Self::ConfigError => "config_error",
        }
    }

    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Pass | Self::Flaky => 0,
            Self::Failed | Self::TimedOut => 1,
            Self::ConfigError => 2,
        }
    }
}

/// Compact `<= 128 B` failure witness summarizing the first failing test or collection error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashCapsule {
    pub witness_digest: String,
    pub gate_name: String,
    pub summary: String,
}

impl CrashCapsule {
    #[must_use]
    pub fn from_failure(gate_name: impl Into<String>, nodeid: &str, detail: &str) -> Self {
        let raw = if detail.is_empty() { nodeid } else { detail };
        let witness_digest = blake3::hash(raw.as_bytes()).to_hex().to_string();
        let line = first_useful_line(raw);
        let combined = format!("{nodeid} :: {line}");
        let summary = truncate_utf8_bytes(&combined, MAX_CRASH_CAPSULE_BYTES);
        Self {
            witness_digest,
            gate_name: gate_name.into(),
            summary,
        }
    }
}

/// Compact `<= 2,048 B` structured execution/planning receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactReceipt {
    pub schema: String,
    pub verdict: ExecVerdict,
    pub tier: ConfidenceTier,
    pub role: SlotRole,
    pub env_fingerprint: String,
    pub wall_ms: u64,
    pub tests_total: usize,
    pub tests_selected: usize,
    pub tests_cached: usize,
    pub tests_tainted: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    pub tests_skipped: usize,
    pub tests_flaky: usize,
    pub worker_restarts: u32,
    pub fast_path_tests: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crash_capsule: Option<CrashCapsule>,
    pub summary: String,
}

impl CompactReceipt {
    pub const SCHEMA_V1: &'static str = "pyrn.receipt.v1";

    /// Serialize to JSON guaranteed to stay within `MAX_RECEIPT_BYTES` (2,048 bytes).
    #[must_use]
    pub fn to_bounded_json(&self) -> Vec<u8> {
        let mut copy = self.clone();
        if copy.env_fingerprint.len() > 16 {
            copy.env_fingerprint.truncate(16);
        }
        let mut bytes = serde_json::to_vec(&copy).unwrap_or_default();
        if bytes.len() > MAX_RECEIPT_BYTES {
            copy.summary = truncate_utf8_bytes(&copy.summary, 96);
            if let Some(ref mut cap) = copy.crash_capsule {
                cap.summary = truncate_utf8_bytes(&cap.summary, 64);
            }
            bytes = serde_json::to_vec(&copy).unwrap_or_default();
        }
        bytes
    }
}

/// Structured NDJSON streaming event emitted to stdout when `--ndjson` is enabled.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    RunStarted {
        mode: String,
        role: SlotRole,
        tier: ConfidenceTier,
        project_root: String,
        workers: usize,
        files_discovered: usize,
        units_scheduled: usize,
        tests_cached: usize,
    },
    TestFinished {
        nodeid: String,
        outcome: TestOutcome,
        duration_ms: f64,
        worker_id: String,
        fast_path: bool,
    },
    WorkerFault {
        worker_id: String,
        kind: String,
        nodeid: Option<String>,
        requeued: usize,
    },
    RunFinished {
        verdict: ExecVerdict,
        exit_code: i32,
        wall_ms: u64,
        passed: usize,
        cached: usize,
        failed: usize,
        skipped: usize,
        flaky: usize,
        timed_out: usize,
        crashed: usize,
    },
}

/// Structured integer metrics block formatted as `<PREFIX> key=<i64>` lines for CI telemetry.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunMetrics {
    pub tests_total: i64,
    pub tests_selected: i64,
    pub tests_cached: i64,
    pub tests_tainted: i64,
    pub tests_passed: i64,
    pub tests_failed: i64,
    pub tests_skipped: i64,
    pub tests_xfailed: i64,
    pub tests_xpassed: i64,
    pub tests_flaky: i64,
    pub tests_timed_out: i64,
    pub tests_crashed: i64,
    pub fast_path_tests: i64,
    pub workers_selected: i64,
    pub worker_restarts: i64,
    pub db_template_hits: i64,
    pub cache_hit_pct: i64,
    pub tia_parse_ms: i64,
    pub tia_graph_ms: i64,
    pub wall_ms: i64,
    pub discovery_ms: i64,
    pub pool_ready_ms: i64,
}

impl RunMetrics {
    /// Format as a single `<prefix> k=v ...` line (falls back to `DEFAULT_METRICS_PREFIX` if empty).
    #[must_use]
    pub fn to_kv_line(&self, prefix: &str) -> String {
        let p = prefix.trim();
        let p = if p.is_empty() {
            DEFAULT_METRICS_PREFIX
        } else {
            p
        };
        format!(
            "{p} tests_total={} tests_selected={} tests_cached={} tests_tainted={} tests_passed={} tests_failed={} tests_skipped={} tests_xfailed={} tests_xpassed={} tests_flaky={} tests_timed_out={} tests_crashed={} fast_path_tests={} workers_selected={} worker_restarts={} db_template_hits={} cache_hit_pct={} tia_parse_ms={} tia_graph_ms={} wall_ms={} discovery_ms={} pool_ready_ms={}",
            self.tests_total,
            self.tests_selected,
            self.tests_cached,
            self.tests_tainted,
            self.tests_passed,
            self.tests_failed,
            self.tests_skipped,
            self.tests_xfailed,
            self.tests_xpassed,
            self.tests_flaky,
            self.tests_timed_out,
            self.tests_crashed,
            self.fast_path_tests,
            self.workers_selected,
            self.worker_restarts,
            self.db_template_hits,
            self.cache_hit_pct,
            self.tia_parse_ms,
            self.tia_graph_ms,
            self.wall_ms,
            self.discovery_ms,
            self.pool_ready_ms,
        )
    }

    /// Format as multi-line `<prefix> key=val` pairs (falls back to `DEFAULT_METRICS_PREFIX` if empty).
    #[must_use]
    pub fn to_kv_lines(&self, prefix: &str) -> String {
        let p = prefix.trim();
        let p = if p.is_empty() {
            DEFAULT_METRICS_PREFIX
        } else {
            p
        };
        format!(
            "{p} tests_total={}\n\
             {p} tests_selected={}\n\
             {p} tests_cached={}\n\
             {p} tests_tainted={}\n\
             {p} tests_passed={}\n\
             {p} tests_failed={}\n\
             {p} tests_skipped={}\n\
             {p} tests_flaky={}\n\
             {p} cache_hit_pct={}\n\
             {p} tia_parse_ms={}\n\
             {p} tia_graph_ms={}\n\
             {p} wall_ms={}",
            self.tests_total,
            self.tests_selected,
            self.tests_cached,
            self.tests_tainted,
            self.tests_passed,
            self.tests_failed,
            self.tests_skipped,
            self.tests_flaky,
            self.cache_hit_pct,
            self.tia_parse_ms,
            self.tia_graph_ms,
            self.wall_ms,
        )
    }
}

/// Extract the most informative error line from a traceback or message.
#[must_use]
pub fn first_useful_line(text: &str) -> &str {
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("E   ")
            || t.contains("AssertionError")
            || t.contains("Error:")
            || t.contains("timed out")
            || t.contains("crashed")
        {
            return t;
        }
    }
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("test failure")
}

/// Truncate a UTF-8 string safely to at most `max_bytes` bytes (never splitting a codepoint).
#[must_use]
pub fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}
