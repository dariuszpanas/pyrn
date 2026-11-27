#![forbid(unsafe_code)]

//! `pyrn` — Unified Rust CLI for Python/pytest Test Impact Analysis (`pyrn-core`),
//! content-addressed test caching (`CasTestCache`), and isolated warm-pool execution (`pyrn-runner`).
//!
//! Exit code contract:
//! - `0`: All tests passed (or passed on rerun as `flaky`), or informational command succeeded.
//! - `1`: Deterministic test failure, timeout, or crash (`failed` / `timed_out`).
//! - `2`: Configuration, CLI argument, or operational/runtime error.

mod presentation;
mod self_manage;

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use clap_complete::Shell;
use presentation::RichOutput;
use pyrn_core::{
    CasTestCache, ChangedFileSpec, ConfidenceTier, DEFAULT_METRICS_PREFIX, DependencyGraph,
    DynamicTraceIndex, ExecVerdict, GraphConfig, InMemoryTree, PlanOptions, PyrnConfig, SlotRole,
    git_changed_specs_against_base, plan_python_tests, replay_git_history,
};
use pyrn_runner::{
    CacheLayout, DaemonResponse, DaemonServeConfig, RunConfig, TimingHistory, UnifiedRunOptions,
    build_compact_receipt, clean_canonical_path, collect_doctor_report, compute_migrations_hash,
    daemon_status, daemon_stop, emit_metrics_kv, execute_unified_run, format_metrics_kv,
    serve_daemon,
};
use self_manage::{inspect_self_status, run_self_update};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "pyrn",
    version,
    about = "Rust-powered Python/pytest TIA, CAS test cache, and isolated warm execution engine"
)]
struct Cli {
    /// Disable ANSI styling and Unicode box borders in human-readable output.
    #[arg(long, global = true)]
    plain: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Execute tests using isolated cold or warm-daemon workers with optional TIA and CAS caching.
    Run(Box<RunArgs>),
    /// Run pure in-memory symbol-level Python Test Impact Analysis without executing tests.
    Plan(PlanArgs),
    /// Manage the persistent warm Python worker pool daemon (`start`, `status`, `stop`).
    Daemon(DaemonArgs),
    /// Inspect or prune the multi-process SQLite WAL content-addressed test cache.
    Cache(CacheArgs),
    /// Inspect effective `.pyrn.toml` / `pyproject.toml` settings or scaffold a starter `.pyrn.toml`.
    Config(ConfigArgs),
    /// Diagnose host/container worker budget, Python/sysmon environment, cache, and slow-test history.
    Doctor(DoctorArgs),
    /// Replay recent Git commit history (read-only) to measure TIA recall and duration-weighted speedup.
    ReplayGit(ReplayGitArgs),
    /// Inspect installation channel (`status`) or update `pyrn` in place (`update`).
    #[command(name = "self")]
    SelfCmd(SelfArgs),
    /// Generate shell completion scripts (`bash`, `zsh`, `fish`, `powershell`, `elvish`).
    Completion(CompletionArgs),
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Target repository root directory.
    #[arg(long, alias = "project", default_value = ".")]
    repo: PathBuf,

    /// Python interpreter executable (auto-detects `.venv` when omitted).
    #[arg(long, env = "PYRN_PYTHON")]
    python: Option<String>,

    /// Enable symbol-level Test Impact Analysis (`pyrn-core`) to run only impacted tests.
    #[arg(long)]
    tia: bool,

    /// Git base ref for TIA symbol diffing (e.g., `HEAD`, `main`, `origin/main`). Implies `--tia`.
    #[arg(long)]
    base: Option<String>,

    /// Explicit changed repository-relative paths for TIA (repeatable or comma-separated). Implies `--tia`.
    #[arg(long = "changed", value_delimiter = ',')]
    changed: Vec<String>,

    /// Enable SQLite WAL content-addressed test result caching (`CasTestCache`).
    #[arg(long)]
    cache: bool,

    /// Route test execution through a running warm persistent daemon (`pyrn daemon start`).
    #[arg(long)]
    daemon: bool,

    /// Cache & daemon isolation role (`agent` or `verify`).
    #[arg(long, default_value = "agent")]
    role: String,

    /// Confidence tier (`strict`, `transitive`, or `full`).
    #[arg(long, default_value = "strict")]
    tier: String,

    /// Worker count: `auto` (physical cores + cgroup CPU/memory OOM budget) or explicit integer `N`.
    #[arg(short = 'w', long = "workers", default_value = "auto")]
    workers: String,

    /// Enable built-in Python 3.12+ `COVERAGE_CORE=sysmon` coverage capture + shard merge.
    #[arg(long)]
    cov_sysmon: bool,

    /// Output path for merged `--cov-sysmon` `.coverage` file.
    #[arg(long)]
    cov_out: Option<PathBuf>,

    /// Emit `<PREFIX> key=<i64>` metrics line to stdout for CI telemetry parsers.
    #[arg(long)]
    metrics_kv: bool,

    /// Prefix used for `--metrics-kv` key-value metrics output.
    #[arg(long, default_value = DEFAULT_METRICS_PREFIX, env = "PYRN_METRICS_PREFIX")]
    metrics_prefix: String,

    /// Write bounded `<= 2,048 B` `CompactReceipt` JSON to this path.
    #[arg(long)]
    receipt_out: Option<PathBuf>,

    /// Write full `RunReport` JSON to this path.
    #[arg(long)]
    json_out: Option<PathBuf>,

    /// Print structured JSON summary (receipt + budget + metrics) to stdout instead of human output.
    #[arg(long)]
    json: bool,

    /// Stream structured `StreamEvent` NDJSON lines to stdout.
    #[arg(long)]
    ndjson: bool,

    /// Auto-rerun failing tests up to N times to detect flakes (`ExecVerdict::Flaky`).
    #[arg(long, default_value_t = 0)]
    reruns: u32,

    /// Hard per-test watchdog timeout in seconds (kills hung worker process tree & respawns).
    #[arg(long, default_value_t = 60)]
    test_timeout: u64,

    /// Pytest marker expression (`-m`).
    #[arg(short = 'm', long = "markers")]
    markers: Option<String>,

    /// Pytest keyword expression (`-k`).
    #[arg(short = 'k', long = "keyword")]
    keyword: Option<String>,

    /// Explicit pytest config file (`-c`).
    #[arg(short = 'c', long = "config-file")]
    config_file: Option<PathBuf>,

    /// Override base cache directory for CAS DB, daemon state, timing history, and SQLite templates.
    #[arg(long, env = "PYRN_CACHE_DIR")]
    cache_dir: Option<PathBuf>,

    /// Collect PEP 669 + PEP 578 + `os.environ` dynamic traces inside workers.
    #[arg(long)]
    trace: bool,

    /// Pre-existing dynamic trace JSON file to refine TIA edges & cache keys.
    #[arg(long)]
    trace_in: Option<PathBuf>,

    /// Write collected dynamic trace JSON to this path.
    #[arg(long)]
    trace_out: Option<PathBuf>,

    /// Dynamic tracing granularity (`file` or `symbol`).
    #[arg(long, default_value = "file")]
    trace_granularity: String,

    /// Enable per-symbol tracing when `--trace` is active.
    #[arg(long)]
    trace_symbols: bool,

    /// Commit SHA metadata recorded when seeding CAS cache entries.
    #[arg(long)]
    commit_sha: Option<String>,

    /// Disable direct fast-path execution (route every test through full pytest protocol).
    #[arg(long)]
    no_fast_path: bool,

    /// Disable heavy-file sharding in the affinity LPT scheduler.
    #[arg(long)]
    no_shard_heavy: bool,

    /// Disable SQLite `:memory:` and file-backed migration template cloning.
    #[arg(long)]
    no_sqlite_template: bool,

    /// Number of slowest tests/files to show in human diagnostics (`0` disables).
    #[arg(long, default_value_t = 10)]
    top_slow: usize,

    /// Maximum failures/flakes to display in human diagnostics.
    #[arg(long, default_value_t = 20)]
    show_failures: usize,

    /// Optional target test files/directories (plus extra pytest args after `--`).
    #[arg(trailing_var_arg = true)]
    targets_and_extra: Vec<String>,
}

#[derive(Args, Debug)]
struct PlanArgs {
    /// Target repository root directory.
    #[arg(long, alias = "project", default_value = ".")]
    repo: PathBuf,

    /// Git base ref to diff working tree against (e.g., `HEAD`, `main`).
    #[arg(long)]
    base: Option<String>,

    /// Explicit changed repository-relative paths (repeatable or comma-separated).
    #[arg(long = "changed", value_delimiter = ',')]
    changed: Vec<String>,

    /// Confidence tier (`strict`, `transitive`, or `full`).
    #[arg(long, default_value = "strict")]
    tier: String,

    /// Slot role recorded in the plan receipt (`agent` or `verify`).
    #[arg(long, default_value = "agent")]
    role: String,

    /// Optional dynamic trace JSON file to include runtime asset/import/env edges.
    #[arg(long)]
    trace_in: Option<PathBuf>,

    /// Emit structured JSON plan to stdout.
    #[arg(long)]
    json: bool,

    /// Emit `<PREFIX> key=<i64>` metrics line to stdout.
    #[arg(long)]
    metrics_kv: bool,

    /// Prefix used for `--metrics-kv` key-value metrics output.
    #[arg(long, default_value = DEFAULT_METRICS_PREFIX, env = "PYRN_METRICS_PREFIX")]
    metrics_prefix: String,

    /// Write bounded `<= 2,048 B` `CompactReceipt` JSON to this path.
    #[arg(long)]
    receipt_out: Option<PathBuf>,

    /// Print every selected test file in human mode (default prints up to 30).
    #[arg(long)]
    all_files: bool,
}

#[derive(Args, Debug)]
struct DaemonArgs {
    #[command(subcommand)]
    action: DaemonAction,
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    /// Start the persistent warm worker pool daemon in the foreground (until `pyrn daemon stop` or Ctrl-C).
    Start {
        #[arg(long, alias = "project", default_value = ".")]
        repo: PathBuf,
        #[arg(long, env = "PYRN_PYTHON")]
        python: Option<String>,
        #[arg(short = 'w', long = "workers", default_value = "auto")]
        workers: String,
        #[arg(long, default_value = "agent")]
        role: String,
        #[arg(long, env = "PYRN_CACHE_DIR")]
        cache_dir: Option<PathBuf>,
        #[arg(short = 'm', long = "markers")]
        markers: Option<String>,
        #[arg(short = 'c', long = "config-file")]
        config_file: Option<PathBuf>,
        #[arg(long)]
        no_sqlite_template: bool,
        #[arg(last = true)]
        extra_pytest_args: Vec<String>,
    },
    /// Query status of the active warm daemon for this `(role, repo, python)`.
    Status {
        #[arg(long, alias = "project", default_value = ".")]
        repo: PathBuf,
        #[arg(long, env = "PYRN_PYTHON")]
        python: Option<String>,
        #[arg(long, default_value = "agent")]
        role: String,
        #[arg(long, env = "PYRN_CACHE_DIR")]
        cache_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Gracefully stop the active warm daemon and terminate all its Python workers.
    Stop {
        #[arg(long, alias = "project", default_value = ".")]
        repo: PathBuf,
        #[arg(long, env = "PYRN_PYTHON")]
        python: Option<String>,
        #[arg(long, default_value = "agent")]
        role: String,
        #[arg(long, env = "PYRN_CACHE_DIR")]
        cache_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
struct CacheArgs {
    #[command(subcommand)]
    action: CacheAction,
}

#[derive(Subcommand, Debug)]
enum CacheAction {
    /// Show SQLite WAL content-addressed test cache statistics and disk footprint.
    Status {
        #[arg(long, alias = "project", default_value = ".")]
        repo: PathBuf,
        #[arg(long, env = "PYRN_PYTHON")]
        python: Option<String>,
        #[arg(long, default_value = "agent")]
        role: String,
        #[arg(long, env = "PYRN_CACHE_DIR")]
        cache_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Prune stale cache entries older than `--max-age-days` (`0` clears all matching entries).
    Prune {
        #[arg(long, alias = "project", default_value = ".")]
        repo: PathBuf,
        #[arg(long, env = "PYRN_PYTHON")]
        python: Option<String>,
        #[arg(long, default_value = "agent")]
        role: String,
        #[arg(long, env = "PYRN_CACHE_DIR")]
        cache_dir: Option<PathBuf>,
        /// Maximum entry age in days (`0` deletes all matching entries).
        #[arg(long, default_value_t = 30)]
        max_age_days: u64,
        /// Restrict pruning strictly to `--role` (`agent` or `verify`) instead of all roles.
        #[arg(long)]
        only_role: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
struct DoctorArgs {
    /// Target repository root directory.
    #[arg(long, alias = "project", default_value = ".")]
    repo: PathBuf,
    /// Python interpreter executable (auto-detects `.venv` when omitted).
    #[arg(long, env = "PYRN_PYTHON")]
    python: Option<String>,
    /// Cache & daemon isolation role (`agent` or `verify`).
    #[arg(long, default_value = "agent")]
    role: String,
    /// Override base cache directory.
    #[arg(long, env = "PYRN_CACHE_DIR")]
    cache_dir: Option<PathBuf>,
    /// Number of slowest historical test files to report from timing history.
    #[arg(long, default_value_t = 10)]
    top_slow: usize,
    /// Emit structured JSON diagnostics to stdout.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ReplayGitArgs {
    /// Target Git repository root directory (strictly read-only).
    #[arg(long, alias = "project", default_value = ".")]
    repo: PathBuf,
    /// Number of recent non-merge commits to replay.
    #[arg(long, default_value_t = 30)]
    limit: usize,
    /// Confidence tier (`strict`, `transitive`, or `full`).
    #[arg(long, default_value = "strict")]
    tier: String,
    /// Optional dynamic trace JSON file to enrich static TIA edges.
    #[arg(long)]
    trace_in: Option<PathBuf>,
    /// Optional `per_file_timings.csv` (`file_path,duration_s` or `file,total_ms`) for duration-weighted speedup.
    #[arg(long)]
    durations_csv: Option<PathBuf>,
    /// Emit full `ReplaySummary` JSON to stdout.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ConfigArgs {
    #[command(subcommand)]
    action: ConfigAction,
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Show effective merged configuration from `pyproject.toml` and `.pyrn.toml`.
    Show {
        /// Target repository root directory.
        #[arg(long, alias = "project", default_value = ".")]
        repo: PathBuf,
        /// Emit structured JSON configuration to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Scaffold a documented starter `.pyrn.toml` in the repository root.
    Init {
        /// Target repository root directory.
        #[arg(long, alias = "project", default_value = ".")]
        repo: PathBuf,
        /// Overwrite an existing `.pyrn.toml`.
        #[arg(long)]
        force: bool,
        /// Emit structured JSON confirmation to stdout.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
struct SelfArgs {
    #[command(subcommand)]
    action: SelfAction,
}

#[derive(Subcommand, Debug)]
enum SelfAction {
    /// Show the installed `pyrn` version, executable path, target triple, and install channel.
    Status {
        /// Emit structured JSON status to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Update `pyrn` using the detected installation channel (standalone, `uv tool`, or Cargo).
    Update {
        /// Print the update command that would be executed without modifying the installation.
        #[arg(long)]
        dry_run: bool,
        /// Target release version (e.g., `0.2.0` or `v0.2.0`). Defaults to latest release.
        #[arg(long)]
        version: Option<String>,
        /// Emit structured JSON update result to stdout.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
struct CompletionArgs {
    /// Target shell (`bash`, `zsh`, `fish`, `powershell`, or `elvish`).
    #[arg(value_enum)]
    shell: Shell,
}

fn resolve_repo_root(repo: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(repo)
        .with_context(|| format!("failed to resolve repository path '{}'", repo.display()))?;
    Ok(clean_canonical_path(canonical))
}

fn resolve_python_exe(project_root: &Path, explicit: Option<&str>) -> String {
    if let Some(py) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return py.to_owned();
    }
    let venv_candidates = [
        project_root
            .join(".venv")
            .join("Scripts")
            .join("python.exe"),
        project_root.join(".venv").join("bin").join("python"),
        project_root.join("venv").join("Scripts").join("python.exe"),
        project_root.join("venv").join("bin").join("python"),
    ];
    for cand in venv_candidates {
        if cand.is_file() {
            return cand.to_string_lossy().into_owned();
        }
    }
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        let base = PathBuf::from(venv);
        for rel in [
            Path::new("Scripts").join("python.exe"),
            Path::new("bin").join("python"),
        ] {
            let cand = base.join(rel);
            if cand.is_file() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    if cfg!(windows) {
        "python".to_owned()
    } else {
        "python3".to_owned()
    }
}

fn parse_workers(raw: &str) -> Result<Option<usize>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let parsed: usize = trimmed.parse().map_err(|_| {
        anyhow::anyhow!("invalid --workers '{raw}' (expected 'auto' or positive integer >= 1)")
    })?;
    if parsed == 0 {
        bail!("--workers must be >= 1 (or 'auto')");
    }
    Ok(Some(parsed))
}

fn split_targets_and_extra(items: &[String]) -> (Vec<String>, Vec<String>) {
    let mut targets = Vec::new();
    let mut extra = Vec::new();
    let mut in_extra = false;
    for item in items {
        if !in_extra && item == "--" {
            in_extra = true;
            continue;
        }
        if in_extra || item.starts_with('-') {
            extra.push(item.clone());
        } else {
            targets.push(item.clone());
        }
    }
    (targets, extra)
}

fn collect_env_map() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

fn write_bytes_creating_parents(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir '{}'", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("write '{}'", path.display()))
}

fn load_file_durations_csv(path: &Path) -> Result<BTreeMap<String, f64>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("read durations CSV '{}'", path.display()))?;
    let mut map = BTreeMap::new();
    let mut lines = text.lines();
    let header = lines.next().unwrap_or("");
    let cols: Vec<&str> = header.split(',').map(str::trim).collect();
    let file_idx = cols
        .iter()
        .position(|c| matches!(*c, "file" | "file_path" | "test_file" | "path"))
        .unwrap_or(0);
    let dur_idx = cols
        .iter()
        .position(|c| {
            matches!(
                *c,
                "total_ms" | "duration_ms" | "wall_ms" | "total_s" | "duration_s" | "seconds"
            )
        })
        .unwrap_or(1);
    let is_seconds = cols
        .get(dur_idx)
        .is_some_and(|c| c.ends_with("_s") || *c == "seconds");

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
        let (Some(f), Some(v_str)) = (parts.get(file_idx), parts.get(dur_idx)) else {
            continue;
        };
        if let Ok(v) = v_str.parse::<f64>() {
            let norm = pyrn_core::normalize_rel_path(f);
            let ms = if is_seconds { v * 1000.0 } else { v };
            map.insert(norm, ms);
        }
    }
    Ok(map)
}

async fn cmd_run(args: RunArgs, plain: bool) -> Result<i32> {
    let project_root = resolve_repo_root(&args.repo)?;
    let python_exe = resolve_python_exe(&project_root, args.python.as_deref());
    let role = SlotRole::parse(&args.role)?;
    let tier = ConfidenceTier::parse(&args.tier)?;
    let workers = parse_workers(&args.workers)?;
    let (targets, extra_pytest_args) = split_targets_and_extra(&args.targets_and_extra);
    let tia_enabled = args.tia || args.base.is_some() || !args.changed.is_empty();

    let opts = UnifiedRunOptions {
        run: RunConfig {
            project_root: project_root.clone(),
            python_exe,
            targets,
            markers: args.markers,
            keyword: args.keyword,
            config_file: args.config_file,
            workers,
            role,
            tier,
            cache_dir: args.cache_dir,
            test_timeout_secs: args.test_timeout,
            reruns: args.reruns,
            fast_path: !args.no_fast_path,
            shard_heavy: !args.no_shard_heavy,
            sqlite_template: !args.no_sqlite_template,
            ndjson: args.ndjson,
            trace: args.trace,
            trace_granularity: args.trace_granularity,
            trace_symbols: args.trace_symbols,
            cov_sysmon: args.cov_sysmon,
            cov_out: args.cov_out,
            extra_pytest_args,
            explicit_nodeids_by_file: None,
        },
        daemon: args.daemon,
        tia: tia_enabled,
        cache: args.cache,
        base_ref: args.base,
        changed_files: args.changed,
        trace_in: args.trace_in,
        trace_out: args.trace_out,
        commit_sha: args.commit_sha,
    };

    let outcome = execute_unified_run(&opts).await?;
    let (receipt, receipt_bytes) = build_compact_receipt(&outcome.report, &outcome.env_fingerprint);

    if let Some(ref receipt_path) = args.receipt_out {
        write_bytes_creating_parents(receipt_path, &receipt_bytes)?;
    }
    if let Some(ref json_path) = args.json_out {
        let full = serde_json::to_vec_pretty(&outcome.report)?;
        write_bytes_creating_parents(json_path, &full)?;
    }

    if args.json {
        let envelope = json!({
            "schema_version": "pyrn.v1",
            "command": "run",
            "ok": outcome.report.exit_code == 0,
            "exit_code": outcome.report.exit_code,
            "receipt": receipt,
            "receipt_bytes": receipt_bytes.len(),
            "cache_seeded": outcome.cache_seeded,
            "resource_budget": outcome.resource_budget,
            "metrics_kv": format_metrics_kv(&outcome.report, &args.metrics_prefix),
            "impact": outcome.impact,
        });
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else if !args.ndjson {
        let mut out = RichOutput::new(false, plain);
        out.render_run_report(
            &outcome.report,
            outcome.cache_seeded,
            args.top_slow,
            args.show_failures,
        );
        print!("{}", out.finish());
    }

    if args.metrics_kv && !args.json {
        emit_metrics_kv(&outcome.report, &args.metrics_prefix);
    }

    Ok(outcome.report.exit_code)
}

fn cmd_plan(args: PlanArgs, plain: bool) -> Result<i32> {
    let project_root = resolve_repo_root(&args.repo)?;
    let role = SlotRole::parse(&args.role)?;
    let tier = ConfidenceTier::parse(&args.tier)?;

    let tree = InMemoryTree::load_from_fs(&project_root)
        .with_context(|| format!("load repo snapshot '{}'", project_root.display()))?;
    let cfg = PyrnConfig::from_tree(&tree);

    let dynamic = match args.trace_in {
        Some(ref p) => Some(
            DynamicTraceIndex::load_json(p)
                .with_context(|| format!("load dynamic traces '{}'", p.display()))?,
        ),
        None => None,
    };

    let explicit_changed: Option<Vec<ChangedFileSpec>> = if !args.changed.is_empty() {
        Some(
            args.changed
                .iter()
                .map(|p| ChangedFileSpec::all(p.as_str()))
                .collect(),
        )
    } else if let Some(ref base_ref) = args.base {
        Some(git_changed_specs_against_base(
            &project_root,
            base_ref,
            &cfg,
        )?)
    } else {
        None
    };

    let mut opts = PlanOptions {
        tier,
        role,
        env_values: collect_env_map(),
        explicit_changed,
        ..PlanOptions::default()
    };
    if let Ok(dsm) = std::env::var("DJANGO_SETTINGS_MODULE")
        && !dsm.is_empty()
    {
        opts.django_settings_module = dsm;
    }

    let plan = plan_python_tests(None, &tree, dynamic.as_ref(), &opts);
    let receipt_bytes = plan.receipt.to_bounded_json();

    if let Some(ref receipt_path) = args.receipt_out {
        write_bytes_creating_parents(receipt_path, &receipt_bytes)?;
    }

    if args.json {
        let out = json!({
            "schema_version": "pyrn.v1",
            "command": "plan",
            "ok": true,
            "env_fingerprint": plan.env_fingerprint,
            "project_fingerprint": plan.project_fingerprint,
            "parse_and_hash_ms": plan.parse_and_hash_ms,
            "graph_build_ms": plan.graph_build_ms,
            "impact": plan.impact,
            "selected_files": plan.selected_files,
            "selected_nodeids_count": plan.selected_nodeids.len(),
            "cache_keys_count": plan.cache_keys.len(),
            "metrics_kv": plan.metrics.to_kv_line(&args.metrics_prefix),
            "receipt": plan.receipt,
            "receipt_bytes": receipt_bytes.len(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        let mut out = RichOutput::new(false, plain);
        out.render_plan_report(&plan, role, args.all_files);
        print!("{}", out.finish());
        if args.metrics_kv {
            println!("{}", plan.metrics.to_kv_line(&args.metrics_prefix));
        }
    }

    Ok(ExecVerdict::Pass.exit_code())
}

async fn cmd_daemon(args: DaemonArgs) -> Result<i32> {
    match args.action {
        DaemonAction::Start {
            repo,
            python,
            workers,
            role,
            cache_dir,
            markers,
            config_file,
            no_sqlite_template,
            extra_pytest_args,
        } => {
            let project_root = resolve_repo_root(&repo)?;
            let python_exe = resolve_python_exe(&project_root, python.as_deref());
            let role = SlotRole::parse(&role)?;
            let workers = parse_workers(&workers)?;
            serve_daemon(DaemonServeConfig {
                project_root,
                python_exe,
                config_file,
                markers,
                workers,
                role,
                cache_dir,
                sqlite_template: !no_sqlite_template,
                extra_pytest_args,
            })
            .await?;
            Ok(0)
        }
        DaemonAction::Status {
            repo,
            python,
            role,
            cache_dir,
            json: as_json,
        } => {
            let project_root = resolve_repo_root(&repo)?;
            let python_exe = resolve_python_exe(&project_root, python.as_deref());
            let role = SlotRole::parse(&role)?;
            match daemon_status(&project_root, &python_exe, role, cache_dir.as_deref()).await {
                Ok(resp) => {
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "schema_version": "pyrn.v1",
                                "command": "daemon.status",
                                "running": true,
                                "daemon": resp,
                            }))?
                        );
                    } else if let DaemonResponse::Status {
                        pid,
                        project_root,
                        role,
                        workers_total,
                        workers_hermetic,
                        workers_sqlite,
                        code_epoch,
                        migrations_hash,
                        runs_served,
                    } = resp
                    {
                        println!(
                            "pyrn daemon RUNNING: pid={pid} role={} workers={workers_total} (hermetic={workers_hermetic}, sqlite={workers_sqlite}) runs_served={runs_served}",
                            role.as_str()
                        );
                        println!(
                            "  project={project_root} epoch={} migrations={}",
                            &code_epoch[..12.min(code_epoch.len())],
                            &migrations_hash[..12.min(migrations_hash.len())]
                        );
                    }
                    Ok(0)
                }
                Err(err) => {
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "schema_version": "pyrn.v1",
                                "command": "daemon.status",
                                "running": false,
                                "detail": err.to_string(),
                            }))?
                        );
                    } else {
                        println!("pyrn daemon STOPPED ({err})");
                    }
                    Ok(1)
                }
            }
        }
        DaemonAction::Stop {
            repo,
            python,
            role,
            cache_dir,
            json: as_json,
        } => {
            let project_root = resolve_repo_root(&repo)?;
            let python_exe = resolve_python_exe(&project_root, python.as_deref());
            let role = SlotRole::parse(&role)?;
            let resp = daemon_stop(&project_root, &python_exe, role, cache_dir.as_deref()).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": "pyrn.v1",
                        "command": "daemon.stop",
                        "ok": true,
                        "response": resp,
                    }))?
                );
            } else {
                println!("pyrn daemon stopped cleanly.");
            }
            Ok(0)
        }
    }
}

fn cmd_cache(args: CacheArgs, plain: bool) -> Result<i32> {
    match args.action {
        CacheAction::Status {
            repo,
            python,
            role,
            cache_dir,
            json: as_json,
        } => {
            let project_root = resolve_repo_root(&repo)?;
            let python_exe = resolve_python_exe(&project_root, python.as_deref());
            let role = SlotRole::parse(&role)?;
            let mig_hash = compute_migrations_hash(&project_root, &python_exe);
            let layout = CacheLayout::prepare(
                cache_dir.as_deref(),
                role,
                &project_root,
                &python_exe,
                &mig_hash,
            )?;
            let cas = CasTestCache::open(&layout.cas_db_path)?;
            let stats = cas.stats()?;
            let db_bytes = fs::metadata(&layout.cas_db_path)
                .map(|m| m.len())
                .unwrap_or(0);
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": "pyrn.v1",
                        "command": "cache.status",
                        "ok": true,
                        "cas_db_path": layout.cas_db_path,
                        "db_size_bytes": db_bytes,
                        "role": role,
                        "stats": stats,
                    }))?
                );
            } else {
                let mut out = RichOutput::new(false, plain);
                out.render_cache_status(role, &layout.cas_db_path, db_bytes, &stats);
                print!("{}", out.finish());
            }
            Ok(0)
        }
        CacheAction::Prune {
            repo,
            python,
            role,
            cache_dir,
            max_age_days,
            only_role,
            json: as_json,
        } => {
            let project_root = resolve_repo_root(&repo)?;
            let python_exe = resolve_python_exe(&project_root, python.as_deref());
            let role = SlotRole::parse(&role)?;
            let mig_hash = compute_migrations_hash(&project_root, &python_exe);
            let layout = CacheLayout::prepare(
                cache_dir.as_deref(),
                role,
                &project_root,
                &python_exe,
                &mig_hash,
            )?;
            let mut cas = CasTestCache::open(&layout.cas_db_path)?;
            let role_filter = only_role.then_some(role);
            let removed = cas.prune_older_than_days(max_age_days, role_filter)?;
            let stats = cas.stats()?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": "pyrn.v1",
                        "command": "cache.prune",
                        "ok": true,
                        "cas_db_path": layout.cas_db_path,
                        "max_age_days": max_age_days,
                        "role_filter": role_filter,
                        "removed": removed,
                        "remaining_stats": stats,
                    }))?
                );
            } else {
                println!(
                    "Pruned {removed} CAS cache entries (max_age_days={max_age_days}, role_filter={:?}); {} remaining.",
                    role_filter.map(SlotRole::as_str),
                    stats.total_entries
                );
            }
            Ok(0)
        }
    }
}

fn describe_config_sources(project_root: &Path) -> String {
    let mut found = Vec::new();
    for name in ["pyproject.toml", ".pyrn.toml", "pyrn.toml"] {
        if project_root.join(name).is_file() {
            found.push(name);
        }
    }
    if found.is_empty() {
        "defaults (no pyproject.toml or .pyrn.toml found)".to_owned()
    } else {
        found.join(" + ")
    }
}

fn cmd_config(args: ConfigArgs, plain: bool) -> Result<i32> {
    match args.action {
        ConfigAction::Show {
            repo,
            json: as_json,
        } => {
            let project_root = resolve_repo_root(&repo)?;
            let cfg = PyrnConfig::load_from_dir(&project_root);
            let sources = describe_config_sources(&project_root);
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": "pyrn.v1",
                        "command": "config.show",
                        "ok": true,
                        "project_root": project_root,
                        "sources": sources,
                        "config": cfg,
                    }))?
                );
            } else {
                let mut out = RichOutput::new(false, plain);
                out.render_config_show(&project_root, &sources, &cfg);
                print!("{}", out.finish());
            }
            Ok(0)
        }
        ConfigAction::Init {
            repo,
            force,
            json: as_json,
        } => {
            let project_root = resolve_repo_root(&repo)?;
            let target = project_root.join(".pyrn.toml");
            if target.exists() && !force {
                bail!(
                    "'{}' already exists (pass --force to overwrite)",
                    target.display()
                );
            }
            let existing = PyrnConfig::load_from_dir(&project_root);
            let testpaths = if existing.testpaths.is_empty() {
                vec!["tests".to_owned()]
            } else {
                existing.testpaths
            };
            let testpaths_toml = testpaths
                .iter()
                .map(|p| format!("{p:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            let starter = format!(
                "# pyrn configuration — https://dariuszpanas.github.io/pyrn/configuration.html\n\
testpaths = [{testpaths_toml}]\n\
python_files = [\"test_*.py\", \"*_test.py\"]\n\
\n\
[tia]\n\
symbol_level_tia = true\n\
conftest_policy = \"smart_fixture_scoped\"\n\
unknown_path_policy = \"full\"\n\
env_allowlist_prefixes = [\"DJANGO_\", \"RAY_\"]\n\
extra_full_triggers = []\n\
\n\
[runner]\n\
worker_mem_mb = 300\n\
pure_autouse_guards = []\n"
            );
            fs::write(&target, starter.as_bytes())
                .with_context(|| format!("write '{}'", target.display()))?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": "pyrn.v1",
                        "command": "config.init",
                        "ok": true,
                        "path": target,
                        "overwritten": force,
                    }))?
                );
            } else {
                println!("Wrote starter configuration to {}", target.display());
            }
            Ok(0)
        }
    }
}

fn cmd_self(args: SelfArgs, plain: bool) -> Result<i32> {
    match args.action {
        SelfAction::Status { json: as_json } => {
            let status = inspect_self_status()?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": "pyrn.v1",
                        "command": "self.status",
                        "ok": true,
                        "status": status,
                    }))?
                );
            } else {
                let mut out = RichOutput::new(false, plain);
                out.render_self_status(&status);
                print!("{}", out.finish());
            }
            Ok(0)
        }
        SelfAction::Update {
            dry_run,
            version,
            json: as_json,
        } => {
            let status = inspect_self_status()?;
            let message = run_self_update(dry_run, version.as_deref())?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": "pyrn.v1",
                        "command": "self.update",
                        "ok": true,
                        "dry_run": dry_run,
                        "requested_version": version,
                        "install_owner": status.install_owner,
                        "message": message,
                    }))?
                );
            } else {
                println!("{message}");
            }
            Ok(0)
        }
    }
}

fn cmd_completion(args: CompletionArgs) -> Result<i32> {
    let mut cmd = Cli::command();
    let mut stdout = std::io::stdout();
    clap_complete::generate(args.shell, &mut cmd, "pyrn", &mut stdout);
    stdout.flush().context("flush completion output")?;
    Ok(0)
}

async fn cmd_doctor(args: DoctorArgs, plain: bool) -> Result<i32> {
    let project_root = resolve_repo_root(&args.repo)?;
    let python_exe = resolve_python_exe(&project_root, args.python.as_deref());
    let role = SlotRole::parse(&args.role)?;

    let report =
        collect_doctor_report(&project_root, &python_exe, role, args.cache_dir.as_deref()).await?;

    let mig_hash = compute_migrations_hash(&project_root, &python_exe);
    let layout = CacheLayout::prepare(
        args.cache_dir.as_deref(),
        role,
        &project_root,
        &python_exe,
        &mig_hash,
    )?;
    let history = TimingHistory::load(&layout.timings_path);
    let mut slowest_files: Vec<(String, f64)> = history
        .file_ms
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    slowest_files.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top_slow_files: Vec<(String, f64)> =
        slowest_files.into_iter().take(args.top_slow).collect();

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": "pyrn.v1",
                "command": "doctor",
                "ok": true,
                "doctor": report,
                "slowest_historical_files_ms": top_slow_files,
            }))?
        );
    } else {
        let mut out = RichOutput::new(false, plain);
        out.render_doctor_report(&report, &top_slow_files);
        print!("{}", out.finish());
    }

    Ok(0)
}

fn cmd_replay_git(args: ReplayGitArgs, plain: bool) -> Result<i32> {
    let project_root = resolve_repo_root(&args.repo)?;
    let tier = ConfidenceTier::parse(&args.tier)?;

    let tree = InMemoryTree::load_from_fs(&project_root)
        .with_context(|| format!("load repo snapshot '{}'", project_root.display()))?;
    let cfg = PyrnConfig::from_tree(&tree);

    let dynamic = match args.trace_in {
        Some(ref p) => Some(
            DynamicTraceIndex::load_json(p)
                .with_context(|| format!("load dynamic traces '{}'", p.display()))?,
        ),
        None => None,
    };

    let (eff_cfg, _) = GraphConfig::from_project(cfg).effective_for_tier(tier);
    let mut graph = DependencyGraph::build(&tree, dynamic.as_ref(), eff_cfg);
    if let Some(ref csv_path) = args.durations_csv {
        graph.file_durations_ms = load_file_durations_csv(csv_path)?;
    }

    let summary = replay_git_history(&project_root, &graph, args.limit, tier)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        let mut out = RichOutput::new(false, plain);
        out.render_replay_git(&summary);
        print!("{}", out.finish());
    }

    Ok(0)
}

async fn run_cli(cli: Cli) -> Result<i32> {
    let plain = cli.plain;
    match cli.command {
        Commands::Run(args) => cmd_run(*args, plain).await,
        Commands::Plan(args) => cmd_plan(args, plain),
        Commands::Daemon(args) => cmd_daemon(args).await,
        Commands::Cache(args) => cmd_cache(args, plain),
        Commands::Config(args) => cmd_config(args, plain),
        Commands::Doctor(args) => cmd_doctor(args, plain).await,
        Commands::ReplayGit(args) => cmd_replay_git(args, plain),
        Commands::SelfCmd(args) => cmd_self(args, plain),
        Commands::Completion(args) => cmd_completion(args),
    }
}

fn raw_args_want_plain() -> bool {
    for arg in std::env::args().skip(1) {
        if arg == "--" {
            break;
        }
        if arg == "--plain" {
            return true;
        }
    }
    false
}

fn find_help_target(root: &clap::Command, raw_args: &[String]) -> clap::Command {
    let mut current = root.clone();
    for token in raw_args {
        if token == "--" {
            break;
        }
        if token.starts_with('-') || token == "help" {
            continue;
        }
        let next = current
            .get_subcommands()
            .find(|s| s.get_name() == token)
            .cloned();
        if let Some(sub) = next {
            current = sub;
        } else {
            break;
        }
    }
    current.build();
    current
}

#[tokio::main]
async fn main() -> ExitCode {
    let raw_plain = raw_args_want_plain();
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(err) => {
            if err.kind() == ErrorKind::DisplayVersion {
                let _ = err.print();
                return ExitCode::SUCCESS;
            }
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                let raw: Vec<String> = std::env::args().skip(1).collect();
                let mut root = Cli::command();
                root.build();
                let target = find_help_target(&root, &raw);
                let is_stderr = err.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand;
                let mut out = RichOutput::new(is_stderr, raw_plain);
                out.render_help(&target);
                if is_stderr {
                    eprint!("{}", out.finish());
                } else {
                    print!("{}", out.finish());
                }
                return ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(2));
            }
            let rendered = err.to_string();
            let mut out = RichOutput::new(true, raw_plain);
            out.render_error("Error", rendered.trim());
            eprint!("{}", out.finish());
            return ExitCode::from(2);
        }
    };

    let plain = cli.plain;
    match run_cli(cli).await {
        Ok(code) => ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(2)),
        Err(err) => {
            if plain {
                eprintln!("pyrn error: {err:#}");
            } else {
                let mut out = RichOutput::new(true, false);
                out.render_error("Error", &format!("pyrn error: {err:#}"));
                eprint!("{}", out.finish());
            }
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unified_run_flags_and_metrics_prefix() {
        let cli = Cli::try_parse_from([
            "pyrn",
            "run",
            "--repo",
            ".",
            "--tia",
            "--base",
            "HEAD",
            "--cache",
            "--daemon",
            "--role",
            "verify",
            "--tier",
            "strict",
            "-w",
            "auto",
            "--cov-sysmon",
            "--metrics-kv",
            "--metrics-prefix",
            "CUSTOM_METRIC",
            "--receipt-out",
            "receipt.json",
            "--ndjson",
            "--reruns",
            "2",
            "--test-timeout",
            "30",
            "-m",
            "not slow",
            "-k",
            "leasing",
            "tests/unit/test_leasing.py",
        ])
        .expect("parse pyrn run");

        match cli.command {
            Commands::Run(r) => {
                assert!(r.tia);
                assert_eq!(r.base.as_deref(), Some("HEAD"));
                assert!(r.cache);
                assert!(r.daemon);
                assert_eq!(r.role, "verify");
                assert_eq!(r.tier, "strict");
                assert_eq!(parse_workers(&r.workers).unwrap(), None);
                assert!(r.cov_sysmon);
                assert!(r.metrics_kv);
                assert_eq!(r.metrics_prefix, "CUSTOM_METRIC");
                assert!(r.ndjson);
                assert_eq!(r.reruns, 2);
                assert_eq!(r.test_timeout, 30);
                assert_eq!(r.markers.as_deref(), Some("not slow"));
                assert_eq!(r.keyword.as_deref(), Some("leasing"));
                assert_eq!(r.targets_and_extra, vec!["tests/unit/test_leasing.py"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Default prefix when `--metrics-prefix` is omitted
        let default_cli = Cli::try_parse_from(["pyrn", "run", "--metrics-kv"])
            .expect("parse default metrics prefix");
        match default_cli.command {
            Commands::Run(r) => {
                assert!(r.metrics_kv);
                assert_eq!(r.metrics_prefix, DEFAULT_METRICS_PREFIX);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_plan_daemon_cache_doctor_replay_subcommands() {
        let plan_cli = Cli::try_parse_from([
            "pyrn",
            "plan",
            "--changed",
            "src/a.py,src/b.py",
            "--metrics-kv",
            "--metrics-prefix",
            "CUSTOM_METRIC",
            "--json",
        ])
        .expect("parse pyrn plan");
        match plan_cli.command {
            Commands::Plan(p) => {
                assert!(p.metrics_kv);
                assert_eq!(p.metrics_prefix, "CUSTOM_METRIC");
                assert_eq!(p.changed, vec!["src/a.py", "src/b.py"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        assert!(
            Cli::try_parse_from(["pyrn", "daemon", "start", "-w", "auto", "--role", "verify"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["pyrn", "daemon", "status", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["pyrn", "daemon", "stop"]).is_ok());
        assert!(Cli::try_parse_from(["pyrn", "cache", "status", "--json"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "pyrn",
                "cache",
                "prune",
                "--max-age-days",
                "7",
                "--only-role"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["pyrn", "doctor", "--json"]).is_ok());
        assert!(
            Cli::try_parse_from(["pyrn", "replay-git", "--limit", "30", "--tier", "strict"])
                .is_ok()
        );
    }

    #[test]
    fn workers_validation_rejects_zero_and_garbage() {
        assert_eq!(parse_workers("auto").unwrap(), None);
        assert_eq!(parse_workers("AUTO").unwrap(), None);
        assert_eq!(parse_workers("8").unwrap(), Some(8));
        assert!(parse_workers("0").is_err());
        assert!(parse_workers("abc").is_err());
    }

    #[test]
    fn plan_command_and_metrics_kv_prefixes_work_end_to_end() {
        let repo = tempfile::tempdir().expect("temp repo");
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::create_dir_all(repo.path().join("tests")).unwrap();
        fs::write(
            repo.path().join(".pyrn.toml"),
            "testpaths = [\"tests\"]\nsymbol_level_tia = true\nworker_mem_mb = 300\n",
        )
        .unwrap();
        fs::write(
            repo.path().join("src/app.py"),
            "def alpha():\n    return 1\n\ndef beta():\n    return 2\n",
        )
        .unwrap();
        fs::write(
            repo.path().join("tests/test_alpha.py"),
            "from src.app import alpha\n\ndef test_a():\n    assert alpha() == 1\n",
        )
        .unwrap();
        fs::write(
            repo.path().join("tests/test_beta.py"),
            "from src.app import beta\n\ndef test_b():\n    assert beta() == 2\n",
        )
        .unwrap();

        // Exercise pure in-memory tree diff + planning + custom metrics prefix
        let mut base = InMemoryTree::load_from_fs(repo.path()).expect("load base tree");
        let mut cand = base.clone();
        cand.insert(
            "src/app.py",
            b"def alpha():\n    return 42\n\ndef beta():\n    return 2\n".to_vec(),
        );
        let plan = plan_python_tests(Some(&base), &cand, None, &PlanOptions::default());
        assert_eq!(plan.selected_files, vec!["tests/test_alpha.py".to_owned()]);
        assert_eq!(plan.receipt.verdict, ExecVerdict::Pass);
        assert!(plan.receipt.to_bounded_json().len() <= pyrn_core::MAX_RECEIPT_BYTES);
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

        // Confirm PyrnConfig loaded `.pyrn.toml` cleanly from tree
        let cfg = PyrnConfig::from_tree(&base);
        assert!(cfg.symbol_level_tia);
        assert_eq!(cfg.worker_mem_mb, Some(300));
        assert_eq!(cfg.testpaths, vec!["tests".to_owned()]);
        base.remove("nonexistent");

        // Exercise `cmd_plan` writing a bounded receipt file with custom metrics prefix
        let receipt_path = repo.path().join("out/receipt.json");
        let code = cmd_plan(
            PlanArgs {
                repo: repo.path().to_path_buf(),
                base: None,
                changed: vec!["src/app.py".to_owned()],
                tier: "strict".to_owned(),
                role: "verify".to_owned(),
                trace_in: None,
                json: false,
                metrics_kv: true,
                metrics_prefix: "CUSTOM_METRIC".to_owned(),
                receipt_out: Some(receipt_path.clone()),
                all_files: true,
            },
            true,
        )
        .expect("cmd_plan");
        assert_eq!(code, 0);
        let written = fs::read(&receipt_path).expect("read receipt");
        assert!(written.len() <= pyrn_core::MAX_RECEIPT_BYTES);
        let parsed: serde_json::Value = serde_json::from_slice(&written).expect("valid json");
        assert_eq!(parsed["verdict"], "pass");
        assert_eq!(parsed["role"], "verify");
    }

    #[tokio::test]
    async fn doctor_command_succeeds_on_temp_repo() {
        let repo = tempfile::tempdir().expect("temp repo");
        let cache = tempfile::tempdir().expect("temp cache");
        fs::write(
            repo.path().join("pyproject.toml"),
            "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
        )
        .unwrap();

        let code = cmd_doctor(
            DoctorArgs {
                repo: repo.path().to_path_buf(),
                python: None,
                role: "agent".to_owned(),
                cache_dir: Some(cache.path().to_path_buf()),
                top_slow: 5,
                json: true,
            },
            true,
        )
        .await
        .expect("cmd_doctor");
        assert_eq!(code, 0);
    }

    #[test]
    fn parses_config_self_completion_and_plain_flag() {
        let cfg_cli =
            Cli::try_parse_from(["pyrn", "--plain", "config", "show", "--json"]).expect("config");
        assert!(cfg_cli.plain);
        assert!(matches!(cfg_cli.command, Commands::Config(_)));
        assert!(Cli::try_parse_from(["pyrn", "config", "init", "--force"]).is_ok());
        assert!(Cli::try_parse_from(["pyrn", "self", "status", "--json"]).is_ok());
        assert!(
            Cli::try_parse_from(["pyrn", "self", "update", "--dry-run", "--version", "0.2.0"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["pyrn", "completion", "bash"]).is_ok());
        assert!(Cli::try_parse_from(["pyrn", "completion", "powershell"]).is_ok());
    }

    #[test]
    fn config_init_and_show_and_rich_help_work() {
        let repo = tempfile::tempdir().expect("temp repo");
        let code = cmd_config(
            ConfigArgs {
                action: ConfigAction::Init {
                    repo: repo.path().to_path_buf(),
                    force: false,
                    json: true,
                },
            },
            true,
        )
        .expect("config init");
        assert_eq!(code, 0);
        assert!(repo.path().join(".pyrn.toml").is_file());

        // Second init without --force must fail cleanly
        assert!(
            cmd_config(
                ConfigArgs {
                    action: ConfigAction::Init {
                        repo: repo.path().to_path_buf(),
                        force: false,
                        json: false,
                    },
                },
                true,
            )
            .is_err()
        );

        let show_code = cmd_config(
            ConfigArgs {
                action: ConfigAction::Show {
                    repo: repo.path().to_path_buf(),
                    json: false,
                },
            },
            true,
        )
        .expect("config show");
        assert_eq!(show_code, 0);

        // Rich help renders Options and Commands panels cleanly
        let mut root = Cli::command();
        root.build();
        let mut out = RichOutput::new(false, false);
        out.render_help(&root);
        let rendered = out.finish();
        assert!(rendered.contains("Options"));
        assert!(rendered.contains("Commands"));
        assert!(rendered.contains("completion"));
        assert!(rendered.contains("config"));
        assert!(rendered.contains("self"));

        // Install-owner detection covers cargo, uv-tool, cargo-build, and standalone
        assert_eq!(
            self_manage::detect_install_owner(Path::new("/home/u/.cargo/bin/pyrn")).0,
            "cargo"
        );
        assert_eq!(
            self_manage::detect_install_owner(Path::new(
                "/home/u/.local/share/uv/tools/pyrn/bin/pyrn"
            ))
            .0,
            "uv-tool"
        );
        assert_eq!(
            self_manage::detect_install_owner(Path::new("C:\\repo\\target\\release\\pyrn.exe")).0,
            "cargo-build"
        );
        assert_eq!(
            self_manage::detect_install_owner(Path::new("/usr/local/bin/pyrn")).0,
            "standalone"
        );
    }
}
