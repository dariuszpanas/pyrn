#![forbid(unsafe_code)]

//! `pyrn-core` — Pure-Rust Test Impact Analysis (TIA), symbol-level dependency graph,
//! content-addressed test caching, and structured verification contracts.
//!
//! ## Feature architecture
//! - **Default features (`[]`)**: 100% pure in-memory Rust — zero subprocesses, zero C/SQLite FFI,
//!   zero filesystem walking. Embeddable directly over any in-memory or virtual tree backend
//!   via [`tree::TreeSource`] and [`policy::plan_python_tests`].
//! - **`fs`**: Parallel filesystem snapshot loader (`InMemoryTree::load_from_fs` / `FsTreeSource`)
//!   and on-disk config/trace loading.
//! - **`cache`**: Multi-process ACID SQLite WAL CAS test cache (`cache::CasTestCache`) with
//!   `(SlotRole::{Agent, Verify}, project_fp, env_fingerprint, cache_key)` isolation.
//! - **`process`**: Read-only Git symbol-diffing and commit-history replay (`git_replay`).

pub mod config;
pub mod dynamic;
pub mod graph;
pub mod key;
pub mod parser;
pub mod policy;
pub mod receipt;
pub mod tree;

#[cfg(feature = "cache")]
pub mod cache;

#[cfg(feature = "process")]
pub mod git_replay;

// Ergonomic top-level re-exports for downstream crates (`pyrn-runner`, `pyrn-cli`, and embedders)
pub use config::{ConftestPolicy, PyrnConfig, UnknownPathPolicy};
pub use dynamic::{
    AggregatedDynamicDeps, DynamicTestTrace, DynamicTraceFile, DynamicTraceIndex, base_nodeid,
};
pub use graph::{
    BuildTimings, CacheSafety, ChangedFileSpec, DependencyGraph, EdgeDetail, EdgeKind, GraphConfig,
    ImpactResult, ImpactedTestFile, SymbolFilter,
};
pub use key::TestCacheKeyRecord;
pub use parser::{
    ChangedSymbols, FixtureDef, ImportRef, ParsedPyFile, SymbolDef, SymbolKind, TestNode,
    diff_python_symbols, parse_python_file,
};
pub use policy::{PlanOptions, TestPlan, diff_trees_with_symbols, plan_python_tests};
pub use receipt::{
    CompactReceipt, ConfidenceTier, CrashCapsule, DEFAULT_METRICS_PREFIX, ExecVerdict,
    MAX_CRASH_CAPSULE_BYTES, MAX_RECEIPT_BYTES, ResourceLane, RunMetrics, SlotRole, StreamEvent,
    TestOutcome, first_useful_line, truncate_utf8_bytes,
};
pub use tree::{
    DOMAIN_AST_BLOB, DOMAIN_AST_PREAMBLE, DOMAIN_AST_SYMBOL, DOMAIN_ENV_FP, DOMAIN_PROJECT_FP,
    DOMAIN_RAW_BLOB, DOMAIN_TEST_KEY, InMemoryTree, TreeSource, blake3_domain_hex,
    normalize_rel_path,
};

#[cfg(feature = "fs")]
pub use tree::FsTreeSource;

#[cfg(feature = "cache")]
pub use cache::{CacheStats, CachedTestEntry, CasTestCache};

#[cfg(feature = "process")]
pub use git_replay::{
    CommitReplayRow, ReplaySummary, git_changed_specs_against_base, git_common_dir,
    replay_git_history,
};
