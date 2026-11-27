//! Async Tokio worker pool supervisor with:
//! - Loopback TCP NDJSON worker control (zero stdout corruption)
//! - Container-aware physical-core resource budgeting (`ResourceBudget`)
//! - Per-test hard watchdog timeouts + process-tree termination (Windows & Unix, zero unsafe)
//! - Crash/segfault isolation with automatic worker respawn & exact remaining-item requeue
//!   (including crash/timeout recovery during `--reruns` flake verification!)
//! - Optional embedded PEP 669 + PEP 578 + `os.environ` dynamic tracing (`--trace`)
//! - Optional built-in `--cov-sysmon` isolated coverage capture + clean combine
//! - Optional live NDJSON event streaming

use crate::discovery::{DiscoveredFile, compute_migrations_hash, discover_test_files};
use crate::isolation::{
    CacheLayout, WorkerSandbox, generate_auth_token, merge_coverage_shards, short_sandbox_root,
};
use crate::protocol::{
    ConfidenceTier, DynamicTestTrace, ExecVerdict, ResourceLane, RunReport, SlotRole, StreamEvent,
    TestOutcome, TestRecord, WorkerCommand, WorkerEvent, WorkerStats,
};
use crate::resources::detect_worker_budget;
use crate::scheduler::{AffinityWorkQueue, TimingHistory, WorkUnit, plan_worker_split};
use anyhow::{Context, Result, anyhow};
use pyrn_core::config::PyrnConfig;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// Maximum worker restarts per slot to guarantee termination (never infinite xdist loops).
const MAX_RESTARTS_PER_SLOT: u32 = 16;

/// Execution configuration shared by cold one-shot runs, warm daemon runs, and unified TIA+CAS runs.
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub project_root: PathBuf,
    pub python_exe: String,
    pub targets: Vec<String>,
    pub markers: Option<String>,
    pub keyword: Option<String>,
    pub config_file: Option<PathBuf>,
    /// `None` enables container-aware physical-core auto-sizing (`-w auto`).
    pub workers: Option<usize>,
    pub role: SlotRole,
    pub tier: ConfidenceTier,
    pub cache_dir: Option<PathBuf>,
    pub test_timeout_secs: u64,
    pub reruns: u32,
    pub fast_path: bool,
    pub shard_heavy: bool,
    pub sqlite_template: bool,
    pub ndjson: bool,
    pub trace: bool,
    pub trace_granularity: String,
    pub trace_symbols: bool,
    pub cov_sysmon: bool,
    pub cov_out: Option<PathBuf>,
    pub extra_pytest_args: Vec<String>,
    /// Optional explicit per-file nodeid allowlist (internal recovery/rerun use).
    pub explicit_nodeids_by_file: Option<BTreeMap<String, Vec<String>>>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            project_root: PathBuf::from("."),
            python_exe: "python".to_owned(),
            targets: Vec::new(),
            markers: None,
            keyword: None,
            config_file: None,
            workers: None,
            role: SlotRole::Agent,
            tier: ConfidenceTier::StrictImpacted,
            cache_dir: None,
            test_timeout_secs: 60,
            reruns: 0,
            fast_path: true,
            shard_heavy: true,
            sqlite_template: true,
            ndjson: false,
            trace: false,
            trace_granularity: "file".to_owned(),
            trace_symbols: false,
            cov_sysmon: false,
            cov_out: None,
            extra_pytest_args: Vec::new(),
            explicit_nodeids_by_file: None,
        }
    }
}

/// Live persistent worker handle (reused across runs in daemon mode or across units in cold mode).
pub struct LiveWorker {
    pub worker_id: String,
    pub slot_index: usize,
    pub lane: ResourceLane,
    pub pid: u32,
    pub boot_ms: f64,
    pub child: Child,
    pub reader: BufReader<OwnedReadHalf>,
    pub writer: OwnedWriteHalf,
    pub sandbox: WorkerSandbox,
    pub stderr_buf: Arc<Mutex<String>>,
    pub restarts: u32,
    /// Repo-relative test paths already imported in this worker's `sys.modules` (sticky affinity).
    pub warm_files: HashSet<String>,
}

impl LiveWorker {
    pub async fn send_cmd(&mut self, cmd: &WorkerCommand) -> Result<()> {
        let mut line = serde_json::to_string(cmd)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn read_event(&mut self) -> Result<Option<WorkerEvent>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let ev: WorkerEvent = serde_json::from_str(line.trim_end())
            .with_context(|| format!("decode worker event: {}", line.trim()))?;
        Ok(Some(ev))
    }

    pub async fn graceful_shutdown(mut self) {
        let _ = self.send_cmd(&WorkerCommand::Shutdown).await;
        let wait_res = tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await;
        if wait_res.is_err() {
            kill_process_tree(self.pid, &mut self.child).await;
        }
    }

    pub async fn hard_kill(&mut self) {
        kill_process_tree(self.pid, &mut self.child).await;
    }

    pub async fn recent_stderr(&self) -> String {
        let guard = self.stderr_buf.lock().await;
        let s = guard.as_str();
        if s.len() > 2048 {
            s[s.len() - 2048..].to_owned()
        } else {
            s.to_owned()
        }
    }
}

/// Context needed to spawn or respawn a worker process.
#[derive(Clone)]
pub struct WorkerSpawnSpec {
    pub project_root: PathBuf,
    pub python_exe: String,
    pub worker_script: PathBuf,
    pub config_file: Option<PathBuf>,
    pub conftest_seed_dir: Option<PathBuf>,
    pub markers: Option<String>,
    pub sqlite_template_path: Option<PathBuf>,
    pub sandbox_parent: PathBuf,
    pub cov_shard_dir: Option<PathBuf>,
    pub cov_sysmon: bool,
    pub pure_autouse_guards: Vec<String>,
    pub extra_pytest_args: Vec<String>,
    pub host: String,
    pub port: u16,
    pub auth_token: String,
}

/// Spawn a single isolated Python worker and wait for its authenticated `Hello` handshake.
pub async fn spawn_worker(
    spec: &WorkerSpawnSpec,
    listener: &Arc<Mutex<TcpListener>>,
    slot_index: usize,
    lane: ResourceLane,
    restarts: u32,
) -> Result<LiveWorker> {
    let worker_id = format!("w{slot_index}-{}", lane.as_str());
    let sandbox = WorkerSandbox::create(
        &spec.sandbox_parent,
        slot_index,
        &worker_id,
        spec.cov_shard_dir.as_deref(),
    )?;

    let cfg_json = build_worker_boot_json(spec, &worker_id, lane, &sandbox);

    let mut cmd = Command::new(&spec.python_exe);
    cmd.arg(&spec.worker_script)
        .arg(&cfg_json)
        .current_dir(&spec.project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    sandbox.apply_env(&mut cmd, spec.cov_sysmon);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn Python worker via {}", spec.python_exe))?;
    let pid = child.id().unwrap_or(0);

    let stderr_buf = attach_pipe_drainers(&mut child);

    let (stream, boot_ms, remote_pid) =
        accept_matching_worker(listener, &spec.auth_token, &worker_id).await?;
    let (read_half, write_half) = stream.into_split();

    Ok(LiveWorker {
        worker_id,
        slot_index,
        lane,
        pid: if remote_pid != 0 { remote_pid } else { pid },
        boot_ms,
        child,
        reader: BufReader::new(read_half),
        writer: write_half,
        sandbox,
        stderr_buf,
        restarts,
        warm_files: HashSet::new(),
    })
}

fn build_worker_boot_json(
    spec: &WorkerSpawnSpec,
    worker_id: &str,
    lane: ResourceLane,
    sandbox: &WorkerSandbox,
) -> String {
    json!({
        "host": spec.host,
        "port": spec.port,
        "auth_token": spec.auth_token,
        "worker_id": worker_id,
        "lane": lane.as_str(),
        "project_root": spec.project_root.to_string_lossy(),
        "config_file": spec.config_file.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "conftest_seed_dir": spec.conftest_seed_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "markers": spec.markers,
        "sqlite_template_path": spec.sqlite_template_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "pure_autouse_guards": spec.pure_autouse_guards,
        "cov_data_file": sandbox.cov_data_file.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "extra_pytest_args": spec.extra_pytest_args,
    })
    .to_string()
}

fn attach_pipe_drainers(child: &mut Child) -> Arc<Mutex<String>> {
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    if let Some(mut err_pipe) = child.stderr.take() {
        let buf_clone = Arc::clone(&stderr_buf);
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err_pipe.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&chunk[..n]);
                let mut g = buf_clone.lock().await;
                g.push_str(&text);
                if g.len() > 8192 {
                    let drop_n = g.len() - 4096;
                    g.drain(..drop_n);
                }
            }
        });
    }
    if let Some(mut out_pipe) = child.stdout.take() {
        tokio::spawn(async move {
            let mut sink = [0u8; 4096];
            while let Ok(n) = out_pipe.read(&mut sink).await {
                if n == 0 {
                    break;
                }
            }
        });
    }
    stderr_buf
}

type PendingConnMap = Arc<Mutex<HashMap<String, (TcpStream, f64, u32)>>>;

/// Spawn a full pool of workers concurrently and pair each connection by `worker_id` + `auth_token`.
pub async fn spawn_worker_pool(
    spec: &WorkerSpawnSpec,
    listener: &Arc<Mutex<TcpListener>>,
    lanes: &[ResourceLane],
) -> Result<Vec<LiveWorker>> {
    let pending: PendingConnMap = Arc::new(Mutex::new(HashMap::new()));
    let expected_count = lanes.len();

    let mut launched = Vec::with_capacity(expected_count);
    for (slot_index, &lane) in lanes.iter().enumerate() {
        let worker_id = format!("w{slot_index}-{}", lane.as_str());
        let sandbox = WorkerSandbox::create(
            &spec.sandbox_parent,
            slot_index,
            &worker_id,
            spec.cov_shard_dir.as_deref(),
        )?;
        let cfg_json = build_worker_boot_json(spec, &worker_id, lane, &sandbox);

        let mut cmd = Command::new(&spec.python_exe);
        cmd.arg(&spec.worker_script)
            .arg(&cfg_json)
            .current_dir(&spec.project_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        sandbox.apply_env(&mut cmd, spec.cov_sysmon);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn Python worker {worker_id}"))?;
        let pid = child.id().unwrap_or(0);
        let stderr_buf = attach_pipe_drainers(&mut child);

        launched.push((slot_index, lane, worker_id, pid, child, sandbox, stderr_buf));
    }

    let accept_fut = async {
        let mut accepted = 0usize;
        while accepted < expected_count {
            let (mut stream, _) = {
                let l = listener.lock().await;
                l.accept().await?
            };
            let _ = stream.set_nodelay(true);
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line))
                .await
                .context("timeout waiting for worker hello")??;
            if n == 0 {
                continue;
            }
            let ev: WorkerEvent = serde_json::from_str(line.trim_end())
                .with_context(|| format!("invalid hello payload: {}", line.trim()))?;
            if let WorkerEvent::Hello {
                auth_token,
                worker_id,
                pid,
                boot_ms,
                ..
            } = ev
            {
                if auth_token != spec.auth_token {
                    continue;
                }
                pending
                    .lock()
                    .await
                    .insert(worker_id, (stream, boot_ms, pid));
                accepted += 1;
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::time::timeout(Duration::from_secs(60), accept_fut)
        .await
        .context("timed out waiting for worker pool startup")??;

    let mut map = pending.lock().await;
    let mut workers = Vec::with_capacity(expected_count);
    for (slot_index, lane, worker_id, pid, child, sandbox, stderr_buf) in launched {
        let (stream, boot_ms, remote_pid) = map
            .remove(&worker_id)
            .ok_or_else(|| anyhow!("missing handshake for worker {worker_id}"))?;
        let (r, w) = stream.into_split();
        workers.push(LiveWorker {
            worker_id,
            slot_index,
            lane,
            pid: if remote_pid != 0 { remote_pid } else { pid },
            boot_ms,
            child,
            reader: BufReader::new(r),
            writer: w,
            sandbox,
            stderr_buf,
            restarts: 0,
            warm_files: HashSet::new(),
        });
    }

    Ok(workers)
}

async fn accept_matching_worker(
    listener: &Arc<Mutex<TcpListener>>,
    expected_token: &str,
    expected_worker_id: &str,
) -> Result<(TcpStream, f64, u32)> {
    let fut = async {
        loop {
            let (mut stream, _) = {
                let l = listener.lock().await;
                l.accept().await?
            };
            let _ = stream.set_nodelay(true);
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                continue;
            }
            if let Ok(WorkerEvent::Hello {
                auth_token,
                worker_id,
                pid,
                boot_ms,
                ..
            }) = serde_json::from_str::<WorkerEvent>(line.trim_end())
                && auth_token == expected_token
                && worker_id == expected_worker_id
            {
                return Ok::<(TcpStream, f64, u32), anyhow::Error>((stream, boot_ms, pid));
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(45), fut)
        .await
        .context("timeout waiting for respawned worker handshake")?
}

/// Kill worker and its entire child process tree (safe, zero `unsafe`).
pub async fn kill_process_tree(pid: u32, child: &mut Child) {
    #[cfg(windows)]
    {
        if pid != 0 {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

/// Execute a cold one-shot test run (spawns fresh isolated pool, executes, shuts down cleanly).
pub async fn execute_cold_run(cfg: &RunConfig) -> Result<RunReport> {
    execute_cold_run_with_exclusions(cfg, None).await
}

/// Execute a cold one-shot test run with an optional per-file negative skip-list of verified CAS hits.
pub async fn execute_cold_run_with_exclusions(
    cfg: &RunConfig,
    exclude_nodeids_by_file: Option<&BTreeMap<String, Vec<String>>>,
) -> Result<RunReport> {
    let t_start = Instant::now();

    let t_disc = Instant::now();
    let pyrn_cfg = PyrnConfig::load_from_dir(&cfg.project_root);
    let files = discover_test_files(
        &cfg.project_root,
        &cfg.targets,
        cfg.markers.as_deref(),
        &pyrn_cfg,
    )?;
    let migrations_hash = compute_migrations_hash(&cfg.project_root, &cfg.python_exe);
    let cache = CacheLayout::prepare(
        cfg.cache_dir.as_deref(),
        cfg.role,
        &cfg.project_root,
        &cfg.python_exe,
        &migrations_hash,
    )?;
    let mut history = TimingHistory::load(&cache.timings_path);
    let discovery_ms = t_disc.elapsed().as_millis() as u64;

    let initial_budget = detect_worker_budget(Some(&cfg.project_root), cfg.workers, None);

    if files.is_empty() {
        return Ok(empty_report(
            "cold",
            cfg.role,
            cfg.tier,
            initial_budget.selected_workers,
            &initial_budget.limiting_factor,
            t_start.elapsed().as_millis() as u64,
            discovery_ms,
        ));
    }

    let (queue, herm_units, sqlite_units) = AffinityWorkQueue::build(
        &files,
        &history,
        initial_budget.selected_workers,
        cfg.shard_heavy,
        cfg.explicit_nodeids_by_file.as_ref(),
        exclude_nodeids_by_file,
    );
    let has_serial = queue.has_serial_external();
    let total_units = queue.remaining();

    if total_units == 0 {
        return Ok(empty_report(
            "cold",
            cfg.role,
            cfg.tier,
            initial_budget.selected_workers,
            &initial_budget.limiting_factor,
            t_start.elapsed().as_millis() as u64,
            discovery_ms,
        ));
    }

    // Re-resolve budget with exact scheduled unit cap so we never spawn idle interpreters.
    let budget = detect_worker_budget(Some(&cfg.project_root), cfg.workers, Some(total_units));
    let effective_workers = budget.selected_workers;

    let (herm_w, sqlite_w) = plan_worker_split(
        &files,
        &history,
        effective_workers,
        herm_units,
        sqlite_units,
    );

    let mut lanes = Vec::with_capacity(herm_w + sqlite_w + usize::from(has_serial));
    for _ in 0..sqlite_w {
        lanes.push(ResourceLane::SqliteDjango);
    }
    for _ in 0..herm_w {
        lanes.push(ResourceLane::Hermetic);
    }
    if has_serial {
        lanes.push(ResourceLane::SerialExternal);
    }

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let listener = Arc::new(Mutex::new(listener));
    let auth_token = generate_auth_token();

    let conftest_seed_dir = find_conftest_seed_dir(&cfg.project_root, &files);
    let config_file = cfg.config_file.clone().or_else(|| {
        let p = cfg.project_root.join("pyproject.toml");
        p.is_file().then_some(p)
    });

    let cov_shard_dir = if cfg.cov_sysmon {
        let d = short_sandbox_root().join(format!(
            "cov-{}-{}",
            std::process::id(),
            &generate_auth_token()[..8]
        ));
        std::fs::create_dir_all(&d)?;
        Some(d)
    } else {
        None
    };

    let spawn_spec = WorkerSpawnSpec {
        project_root: cfg.project_root.clone(),
        python_exe: cfg.python_exe.clone(),
        worker_script: cache.worker_script.clone(),
        config_file,
        conftest_seed_dir,
        markers: cfg.markers.clone(),
        sqlite_template_path: cfg
            .sqlite_template
            .then_some(cache.sqlite_template_path.clone()),
        sandbox_parent: short_sandbox_root(),
        cov_shard_dir: cov_shard_dir.clone(),
        cov_sysmon: cfg.cov_sysmon,
        pure_autouse_guards: pyrn_cfg.pure_autouse_guards.clone(),
        extra_pytest_args: cfg.extra_pytest_args.clone(),
        host: "127.0.0.1".to_owned(),
        port,
        auth_token,
    };

    let t_pool = Instant::now();
    let workers = spawn_worker_pool(&spawn_spec, &listener, &lanes).await?;
    let pool_ready_ms = t_pool.elapsed().as_millis() as u64;

    if cfg.ndjson {
        emit_stream(&StreamEvent::RunStarted {
            mode: "cold".to_owned(),
            role: cfg.role,
            tier: cfg.tier,
            project_root: cfg.project_root.to_string_lossy().into_owned(),
            workers: workers.len(),
            files_discovered: files.len(),
            units_scheduled: total_units,
            tests_cached: 0,
        });
    }

    let (mut report, surviving_workers) = execute_on_pool(
        workers,
        queue,
        &spawn_spec,
        &listener,
        cfg,
        &[],
        "cold",
        &budget.limiting_factor,
        discovery_ms,
        pool_ready_ms,
        t_start,
        cov_shard_dir.as_deref(),
    )
    .await?;

    // Shut down all ephemeral workers in parallel.
    let mut shut_handles = Vec::with_capacity(surviving_workers.len());
    for w in surviving_workers {
        shut_handles.push(tokio::spawn(async move {
            w.graceful_shutdown().await;
        }));
    }
    for h in shut_handles {
        let _ = h.await;
    }

    // Merge isolated per-worker sysmon coverage shards cleanly if `--cov-sysmon` was requested.
    if let Some(ref shard_dir) = cov_shard_dir {
        let target_cov = cfg
            .cov_out
            .clone()
            .unwrap_or_else(|| cache.default_cov_out_path.clone());
        if let Ok(Some(merged)) =
            merge_coverage_shards(&cfg.python_exe, &cfg.project_root, shard_dir, &target_cov).await
        {
            report.coverage_file = Some(merged.to_string_lossy().into_owned());
        } else {
            let _ = std::fs::remove_dir_all(shard_dir);
        }
    }

    report.wall_ms = t_start.elapsed().as_millis() as u64;
    history.update_and_save(&cache.timings_path, &report.results);

    if cfg.ndjson {
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

    Ok(report)
}

/// Core shared pool execution engine used by BOTH cold one-shot runs AND warm persistent daemon.
#[allow(clippy::too_many_arguments)]
pub async fn execute_on_pool(
    workers: Vec<LiveWorker>,
    queue: AffinityWorkQueue,
    spawn_spec: &WorkerSpawnSpec,
    listener: &Arc<Mutex<TcpListener>>,
    cfg: &RunConfig,
    changed_paths: &[String],
    mode: &str,
    worker_limit_reason: &str,
    discovery_ms: u64,
    pool_ready_ms: u64,
    t_start: Instant,
    run_cov_shard_dir: Option<&Path>,
) -> Result<(RunReport, Vec<LiveWorker>)> {
    let workers_requested = workers.len();
    let shared_queue = Arc::new(Mutex::new(queue));
    let shared_results = Arc::new(Mutex::new(Vec::<TestRecord>::new()));
    let shared_traces = Arc::new(Mutex::new(Vec::<DynamicTestTrace>::new()));
    let shared_collect_errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let shared_deselected = Arc::new(AtomicUsize::new(0));
    let next_recovery_id = Arc::new(AtomicUsize::new(1));

    let mut handles = Vec::with_capacity(workers.len());
    for worker in workers {
        let q = Arc::clone(&shared_queue);
        let res_store = Arc::clone(&shared_results);
        let trace_store = Arc::clone(&shared_traces);
        let col_errs = Arc::clone(&shared_collect_errors);
        let desel = Arc::clone(&shared_deselected);
        let rec_seq = Arc::clone(&next_recovery_id);
        let spec = spawn_spec.clone();
        let lst = Arc::clone(listener);
        let markers = cfg.markers.clone();
        let keyword = cfg.keyword.clone();
        let timeout_secs = cfg.test_timeout_secs.max(1);
        let fast_path = cfg.fast_path;
        let ndjson = cfg.ndjson;
        let trace = cfg.trace;
        let trace_granularity = cfg.trace_granularity.clone();
        let trace_symbols = cfg.trace_symbols;
        let changed = changed_paths.to_vec();
        let worker_cov_file = run_cov_shard_dir.map(|d| {
            d.join(format!(".coverage.{}", worker.worker_id))
                .to_string_lossy()
                .into_owned()
        });

        handles.push(tokio::spawn(async move {
            run_worker_loop(
                worker,
                q,
                res_store,
                trace_store,
                col_errs,
                desel,
                rec_seq,
                spec,
                lst,
                markers,
                keyword,
                timeout_secs,
                fast_path,
                ndjson,
                trace,
                trace_granularity,
                trace_symbols,
                worker_cov_file,
                changed,
            )
            .await
        }));
    }

    let mut surviving_workers = Vec::new();
    let mut worker_stats = Vec::new();

    for h in handles {
        let (maybe_worker, stats) = h.await.context("worker supervisor task panicked")?;
        if let Some(w) = maybe_worker {
            surviving_workers.push(w);
        }
        worker_stats.push(stats);
    }

    let mut all_results = {
        let mut g = shared_results.lock().await;
        std::mem::take(&mut *g)
    };
    let mut all_traces = {
        let mut g = shared_traces.lock().await;
        std::mem::take(&mut *g)
    };

    // Automatic flake detection pass (`--reruns N`) with full crash/timeout respawn safety!
    if cfg.reruns > 0 && !surviving_workers.is_empty() {
        let failing_indices: Vec<usize> = all_results
            .iter()
            .enumerate()
            .filter(|(_, r)| r.outcome.is_failure() && r.outcome != TestOutcome::TimedOut)
            .map(|(i, _)| i)
            .collect();

        if !failing_indices.is_empty() {
            let mut rerun_map: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for idx in failing_indices {
                let file = all_results[idx]
                    .nodeid
                    .split("::")
                    .next()
                    .unwrap_or("")
                    .to_owned();
                rerun_map.entry(file).or_default().push(idx);
            }

            let mut worker_pool = surviving_workers;
            for attempt in 1..=cfg.reruns {
                let mut still_failing = false;
                for (file, indices) in &rerun_map {
                    let mut remaining_nodeids: Vec<String> = indices
                        .iter()
                        .filter(|&&i| all_results[i].outcome.is_failure())
                        .map(|&i| all_results[i].nodeid.clone())
                        .collect();
                    if remaining_nodeids.is_empty() {
                        continue;
                    }
                    still_failing = true;
                    let mut sub_seq = 0usize;
                    while !remaining_nodeids.is_empty() {
                        let Some(mut w) = worker_pool.pop() else {
                            break;
                        };
                        sub_seq += 1;
                        let unit = WorkUnit {
                            unit_id: format!("flake-r{attempt}-s{sub_seq}"),
                            lane: w.lane,
                            paths: vec![file.clone()],
                            shard_index: 0,
                            shard_total: 1,
                            explicit_nodeids: Some(remaining_nodeids.clone()),
                            exclude_nodeids: None,
                            estimated_ms: 100.0,
                            retry_attempt: attempt,
                        };
                        let rerun = run_single_unit_for_flake(
                            &mut w,
                            &unit,
                            cfg.markers.as_deref(),
                            cfg.keyword.as_deref(),
                            cfg.test_timeout_secs.max(1),
                            cfg.fast_path,
                            cfg.trace,
                            &cfg.trace_granularity,
                            cfg.trace_symbols,
                        )
                        .await;

                        let mut handled: HashSet<String> = HashSet::new();
                        for (nid, out) in rerun.outcomes {
                            handled.insert(nid.clone());
                            if let Some(rec) = all_results.iter_mut().find(|r| r.nodeid == nid) {
                                rec.attempts += 1;
                                if out == TestOutcome::Passed {
                                    rec.outcome = TestOutcome::Flaky;
                                }
                            }
                        }
                        all_traces.extend(rerun.traces);

                        if rerun.worker_faulted {
                            // Attribute crash/timeout during flake rerun, respawn a clean worker,
                            // and continue rerunning any remaining unexecuted failing nodeids in this file!
                            if let Some(ref blamed) = rerun.blamed_nodeid {
                                handled.insert(blamed.clone());
                                if let Some(rec) =
                                    all_results.iter_mut().find(|r| r.nodeid == *blamed)
                                {
                                    rec.attempts += 1;
                                    if rerun.fault_kind == Some("timed_out") {
                                        rec.outcome = TestOutcome::TimedOut;
                                    } else {
                                        rec.outcome = TestOutcome::Crashed;
                                    }
                                }
                            } else if let Some(first) = remaining_nodeids.first() {
                                // Fault before TestStart: advance past first item to guarantee termination.
                                handled.insert(first.clone());
                            }
                            let slot = w.slot_index;
                            let lane = w.lane;
                            let prev_restarts = w.restarts;
                            let wid = w.worker_id.clone();
                            w.hard_kill().await;
                            if prev_restarts < MAX_RESTARTS_PER_SLOT
                                && let Ok(new_w) = spawn_worker(
                                    spawn_spec,
                                    listener,
                                    slot,
                                    lane,
                                    prev_restarts + 1,
                                )
                                .await
                            {
                                if let Some(st) =
                                    worker_stats.iter_mut().find(|s| s.worker_id == wid)
                                {
                                    st.restarts += 1;
                                }
                                worker_pool.push(new_w);
                            }
                            remaining_nodeids.retain(|nid| !handled.contains(nid));
                        } else {
                            worker_pool.push(w);
                            break;
                        }
                    }
                }
                if !still_failing {
                    break;
                }
            }
            surviving_workers = worker_pool;
        }
    }

    // Build deterministic final report.
    all_results.sort_by(|a, b| a.nodeid.cmp(&b.nodeid));
    worker_stats.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));

    let collect_errors = shared_collect_errors.lock().await.clone();
    let deselected = shared_deselected.load(Ordering::Relaxed);

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut xfailed = 0usize;
    let mut xpassed = 0usize;
    let mut errors = 0usize;
    let mut timed_out = 0usize;
    let mut crashed = 0usize;
    let mut flaky = 0usize;
    let mut cached = 0usize;
    let mut fast_path_tests = 0usize;
    let mut full_pytest_tests = 0usize;

    for r in &all_results {
        match r.outcome {
            TestOutcome::Passed => passed += 1,
            TestOutcome::Failed => failed += 1,
            TestOutcome::Skipped => skipped += 1,
            TestOutcome::Xfailed => xfailed += 1,
            TestOutcome::Xpassed => xpassed += 1,
            TestOutcome::Error => errors += 1,
            TestOutcome::TimedOut => timed_out += 1,
            TestOutcome::Crashed => crashed += 1,
            TestOutcome::Flaky => flaky += 1,
            TestOutcome::Cached => cached += 1,
        }
        if r.fast_path {
            fast_path_tests += 1;
        } else {
            full_pytest_tests += 1;
        }
    }

    let collect_total_ms: f64 = worker_stats.iter().map(|s| s.collect_ms).sum();
    let db_setup_total_ms: f64 = worker_stats.iter().map(|s| s.db_setup_ms).sum();
    let db_template_hits: u32 = worker_stats.iter().map(|s| s.db_template_hits).sum();
    let worker_restarts: u32 = worker_stats.iter().map(|s| s.restarts).sum();

    let verdict = compute_verdict(
        failed,
        errors,
        timed_out,
        crashed,
        flaky,
        !collect_errors.is_empty(),
    );
    let exit_code = verdict.exit_code();

    let report = RunReport {
        mode: mode.to_owned(),
        role: cfg.role,
        tier: cfg.tier,
        workers_requested,
        worker_limit_reason: worker_limit_reason.to_owned(),
        wall_ms: t_start.elapsed().as_millis() as u64,
        discovery_ms,
        pool_ready_ms,
        tia_parse_ms: 0,
        tia_graph_ms: 0,
        collect_total_ms: round3(collect_total_ms),
        db_setup_total_ms: round3(db_setup_total_ms),
        db_template_hits,
        total_collected: all_results.len(),
        tests_selected: all_results.len(),
        tests_cached: cached,
        tests_tainted: 0,
        deselected,
        passed,
        failed,
        skipped,
        xfailed,
        xpassed,
        errors,
        timed_out,
        crashed,
        flaky,
        fast_path_tests,
        full_pytest_tests,
        worker_restarts,
        coverage_file: None,
        collection_errors: collect_errors,
        verdict,
        exit_code,
        worker_stats,
        results: all_results,
        traces: all_traces,
    };

    Ok((report, surviving_workers))
}

/// Determine `ExecVerdict` from failure/timeout/flake counts.
#[must_use]
pub const fn compute_verdict(
    failed: usize,
    errors: usize,
    timed_out: usize,
    crashed: usize,
    flaky: usize,
    has_collect_errors: bool,
) -> ExecVerdict {
    if timed_out > 0 {
        ExecVerdict::TimedOut
    } else if failed > 0 || errors > 0 || crashed > 0 || has_collect_errors {
        ExecVerdict::Failed
    } else if flaky > 0 {
        ExecVerdict::Flaky
    } else {
        ExecVerdict::Pass
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_loop(
    mut worker: LiveWorker,
    queue: Arc<Mutex<AffinityWorkQueue>>,
    results: Arc<Mutex<Vec<TestRecord>>>,
    traces: Arc<Mutex<Vec<DynamicTestTrace>>>,
    collect_errors: Arc<Mutex<Vec<String>>>,
    deselected_counter: Arc<AtomicUsize>,
    recovery_seq: Arc<AtomicUsize>,
    spawn_spec: WorkerSpawnSpec,
    listener: Arc<Mutex<TcpListener>>,
    markers: Option<String>,
    keyword: Option<String>,
    timeout_secs: u64,
    fast_path: bool,
    ndjson: bool,
    trace: bool,
    trace_granularity: String,
    trace_symbols: bool,
    cov_data_file: Option<String>,
    mut pending_changed_paths: Vec<String>,
) -> (Option<LiveWorker>, WorkerStats) {
    let mut stats = WorkerStats {
        worker_id: worker.worker_id.clone(),
        lane: worker.lane.as_str().to_owned(),
        boot_ms: worker.boot_ms,
        ..WorkerStats::default()
    };

    let mut worker_alive = true;
    let active_t0 = Instant::now();
    let mut did_work = false;

    loop {
        let maybe_unit = {
            let mut q = queue.lock().await;
            q.pop_for_sticky(worker.lane, &worker.warm_files)
        };
        let Some(unit) = maybe_unit else {
            break;
        };
        did_work = true;

        let changed_to_send = std::mem::take(&mut pending_changed_paths);
        for p in &changed_to_send {
            worker.warm_files.remove(p);
        }
        let unit_cov = cov_data_file.clone().or_else(|| {
            worker
                .sandbox
                .cov_data_file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
        });
        let cmd = WorkerCommand::RunUnit {
            unit_id: unit.unit_id.clone(),
            paths: unit.paths.clone(),
            markers: markers.clone(),
            keyword: keyword.clone(),
            shard_index: unit.shard_index,
            shard_total: unit.shard_total,
            explicit_nodeids: unit.explicit_nodeids.clone().map(Box::new),
            exclude_nodeids: unit.exclude_nodeids.clone().map(Box::new),
            fast_path,
            changed_paths: changed_to_send,
            trace,
            trace_granularity: trace_granularity.clone(),
            trace_symbols,
            cov_data_file: unit_cov,
        };

        if let Err(send_err) = worker.send_cmd(&cmd).await {
            queue.lock().await.push_front(unit);
            if !try_respawn(
                &mut worker,
                &mut stats,
                &spawn_spec,
                &listener,
                &send_err.to_string(),
            )
            .await
            {
                worker_alive = false;
                break;
            }
            continue;
        }

        let mut collected_nodeids: Vec<String> = Vec::new();
        let mut completed_nodeids: HashSet<String> = HashSet::new();
        let mut current_test: Option<String> = None;
        let mut unit_finished = false;
        let mut fault_kind: Option<(&'static str, String)> = None;

        let collect_deadline = Duration::from_secs((timeout_secs * 2).max(45));
        let test_deadline = Duration::from_secs(timeout_secs);

        while !unit_finished {
            let deadline = if current_test.is_some() {
                test_deadline
            } else {
                collect_deadline
            };

            match tokio::time::timeout(deadline, worker.read_event()).await {
                Ok(Ok(Some(ev))) => match ev {
                    WorkerEvent::UnitCollected {
                        nodeids,
                        deselected,
                        collect_ms,
                        ..
                    } => {
                        stats.collect_ms += collect_ms;
                        deselected_counter.fetch_add(deselected, Ordering::Relaxed);
                        collected_nodeids = nodeids;
                    }
                    WorkerEvent::UnitCollectError {
                        collect_ms, error, ..
                    } => {
                        stats.collect_ms += collect_ms;
                        collect_errors
                            .lock()
                            .await
                            .push(format!("{}: {error}", unit.paths.join(",")));
                        unit_finished = true;
                    }
                    WorkerEvent::TestStart { nodeid, .. } => {
                        current_test = Some(nodeid);
                    }
                    WorkerEvent::TestResult {
                        nodeid,
                        outcome,
                        setup_ms,
                        call_ms,
                        teardown_ms,
                        fast_path: used_fp,
                        longrepr,
                        trace: maybe_trace,
                        ..
                    } => {
                        let total_ms = round3(setup_ms + call_ms + teardown_ms);
                        current_test = None;
                        completed_nodeids.insert(nodeid.clone());
                        stats.tests_run += 1;
                        stats.exec_ms += total_ms;
                        if used_fp {
                            stats.fast_path_tests += 1;
                        }
                        if let Some(tr) = maybe_trace {
                            traces.lock().await.push(*tr);
                        }
                        if ndjson {
                            emit_stream(&StreamEvent::TestFinished {
                                nodeid: nodeid.clone(),
                                outcome,
                                duration_ms: total_ms,
                                worker_id: worker.worker_id.clone(),
                                fast_path: used_fp,
                            });
                        }
                        results.lock().await.push(TestRecord {
                            nodeid,
                            outcome,
                            setup_ms,
                            call_ms,
                            teardown_ms,
                            total_ms,
                            worker_id: worker.worker_id.clone(),
                            lane: worker.lane,
                            fast_path: used_fp,
                            attempts: 1,
                            longrepr,
                        });
                    }
                    WorkerEvent::UnitDone {
                        db_setup_ms,
                        db_template_hit,
                        db_template_hits,
                        ..
                    } => {
                        for p in &unit.paths {
                            worker.warm_files.insert(p.clone());
                        }
                        stats.units_run += 1;
                        stats.db_setup_ms += db_setup_ms;
                        let hit_delta = if db_template_hits > 0 {
                            db_template_hits
                        } else if db_setup_ms > 0.0 && db_template_hit {
                            1
                        } else {
                            0
                        };
                        stats.db_template_hits += hit_delta;
                        unit_finished = true;
                    }
                    WorkerEvent::WorkerError { message } => {
                        collect_errors.lock().await.push(message);
                        unit_finished = true;
                    }
                    _ => {}
                },
                Ok(Ok(None)) => {
                    let stderr_tail = worker.recent_stderr().await;
                    let msg = if stderr_tail.trim().is_empty() {
                        format!("worker {} terminated unexpectedly (EOF)", worker.worker_id)
                    } else {
                        format!(
                            "worker {} crashed unexpectedly:\n{}",
                            worker.worker_id,
                            stderr_tail.trim()
                        )
                    };
                    fault_kind = Some(("crashed", msg));
                    break;
                }
                Ok(Err(err)) => {
                    let stderr_tail = worker.recent_stderr().await;
                    fault_kind = Some((
                        "crashed",
                        format!("worker protocol error: {err}\n{stderr_tail}"),
                    ));
                    break;
                }
                Err(_) => {
                    let msg = match &current_test {
                        Some(nid) => format!(
                            "test `{nid}` exceeded hard watchdog timeout of {timeout_secs}s on worker {}",
                            worker.worker_id
                        ),
                        None => format!(
                            "unit `{}` exceeded collection watchdog timeout of {}s on worker {}",
                            unit.paths.join(","),
                            collect_deadline.as_secs(),
                            worker.worker_id
                        ),
                    };
                    fault_kind = Some(("timed_out", msg));
                    break;
                }
            }
        }

        if let Some((kind, reason)) = fault_kind {
            worker.hard_kill().await;

            let blamed = current_test.clone();
            if let Some(ref culprit_nodeid) = blamed {
                let outcome = if kind == "timed_out" {
                    TestOutcome::TimedOut
                } else {
                    TestOutcome::Crashed
                };
                completed_nodeids.insert(culprit_nodeid.clone());
                let dur = if kind == "timed_out" {
                    (timeout_secs as f64) * 1000.0
                } else {
                    0.0
                };
                results.lock().await.push(TestRecord {
                    nodeid: culprit_nodeid.clone(),
                    outcome,
                    setup_ms: 0.0,
                    call_ms: dur,
                    teardown_ms: 0.0,
                    total_ms: dur,
                    worker_id: worker.worker_id.clone(),
                    lane: worker.lane,
                    fast_path: false,
                    attempts: 1,
                    longrepr: Some(reason.clone()),
                });
            } else {
                collect_errors.lock().await.push(reason.clone());
            }

            let remaining_nodeids: Vec<String> = collected_nodeids
                .into_iter()
                .filter(|nid| !completed_nodeids.contains(nid))
                .collect();
            let requeued_count = remaining_nodeids.len();

            if !remaining_nodeids.is_empty() {
                let rseq = recovery_seq.fetch_add(1, Ordering::Relaxed);
                queue.lock().await.push_front(WorkUnit {
                    unit_id: format!("recover-{rseq:04}"),
                    lane: unit.lane,
                    paths: unit.paths.clone(),
                    shard_index: 0,
                    shard_total: 1,
                    explicit_nodeids: Some(remaining_nodeids),
                    exclude_nodeids: None,
                    estimated_ms: unit.estimated_ms,
                    retry_attempt: unit.retry_attempt + 1,
                });
            }

            if ndjson {
                emit_stream(&StreamEvent::WorkerFault {
                    worker_id: worker.worker_id.clone(),
                    kind: kind.to_owned(),
                    nodeid: blamed,
                    requeued: requeued_count,
                });
            }

            if !try_respawn(&mut worker, &mut stats, &spawn_spec, &listener, &reason).await {
                worker_alive = false;
                break;
            }
        }
    }

    if did_work {
        stats.wall_active_ms = round3(active_t0.elapsed().as_secs_f64() * 1000.0);
    }
    stats.collect_ms = round3(stats.collect_ms);
    stats.db_setup_ms = round3(stats.db_setup_ms);
    stats.exec_ms = round3(stats.exec_ms);

    (worker_alive.then_some(worker), stats)
}

async fn try_respawn(
    worker: &mut LiveWorker,
    stats: &mut WorkerStats,
    spawn_spec: &WorkerSpawnSpec,
    listener: &Arc<Mutex<TcpListener>>,
    _reason: &str,
) -> bool {
    if worker.restarts >= MAX_RESTARTS_PER_SLOT {
        return false;
    }
    let next_restarts = worker.restarts + 1;
    match spawn_worker(
        spawn_spec,
        listener,
        worker.slot_index,
        worker.lane,
        next_restarts,
    )
    .await
    {
        Ok(new_w) => {
            stats.restarts += 1;
            *worker = new_w;
            true
        }
        Err(_) => false,
    }
}

struct FlakeRerunResult {
    outcomes: Vec<(String, TestOutcome)>,
    traces: Vec<DynamicTestTrace>,
    worker_faulted: bool,
    fault_kind: Option<&'static str>,
    blamed_nodeid: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn run_single_unit_for_flake(
    worker: &mut LiveWorker,
    unit: &WorkUnit,
    markers: Option<&str>,
    keyword: Option<&str>,
    timeout_secs: u64,
    fast_path: bool,
    trace: bool,
    trace_granularity: &str,
    trace_symbols: bool,
) -> FlakeRerunResult {
    let mut res = FlakeRerunResult {
        outcomes: Vec::new(),
        traces: Vec::new(),
        worker_faulted: false,
        fault_kind: None,
        blamed_nodeid: None,
    };
    let cmd = WorkerCommand::RunUnit {
        unit_id: unit.unit_id.clone(),
        paths: unit.paths.clone(),
        markers: markers.map(str::to_owned),
        keyword: keyword.map(str::to_owned),
        shard_index: 0,
        shard_total: 1,
        explicit_nodeids: unit.explicit_nodeids.clone().map(Box::new),
        exclude_nodeids: None,
        fast_path,
        changed_paths: Vec::new(),
        trace,
        trace_granularity: trace_granularity.to_owned(),
        trace_symbols,
        cov_data_file: None,
    };
    if worker.send_cmd(&cmd).await.is_err() {
        res.worker_faulted = true;
        res.fault_kind = Some("crashed");
        return res;
    }
    let deadline = Duration::from_secs(timeout_secs);
    let mut current_test: Option<String> = None;
    loop {
        match tokio::time::timeout(deadline, worker.read_event()).await {
            Ok(Ok(Some(ev))) => match ev {
                WorkerEvent::TestStart { nodeid, .. } => {
                    current_test = Some(nodeid);
                }
                WorkerEvent::TestResult {
                    nodeid,
                    outcome,
                    trace: maybe_trace,
                    ..
                } => {
                    current_test = None;
                    res.outcomes.push((nodeid, outcome));
                    if let Some(tr) = maybe_trace {
                        res.traces.push(*tr);
                    }
                }
                WorkerEvent::UnitDone { .. } | WorkerEvent::UnitCollectError { .. } => break,
                _ => {}
            },
            Ok(Ok(None)) | Ok(Err(_)) => {
                res.worker_faulted = true;
                res.fault_kind = Some("crashed");
                res.blamed_nodeid = current_test;
                break;
            }
            Err(_) => {
                res.worker_faulted = true;
                res.fault_kind = Some("timed_out");
                res.blamed_nodeid = current_test;
                break;
            }
        }
    }
    res
}

pub fn find_conftest_seed_dir(project_root: &Path, files: &[DiscoveredFile]) -> Option<PathBuf> {
    let tests_dir = project_root.join("tests");
    if tests_dir.join("conftest.py").is_file() {
        return Some(tests_dir);
    }
    for f in files {
        let mut cur = f.abs_path.parent();
        while let Some(dir) = cur {
            if dir.join("conftest.py").is_file() {
                return Some(dir.to_path_buf());
            }
            if dir == project_root {
                break;
            }
            cur = dir.parent();
        }
    }
    None
}

pub fn emit_stream(ev: &StreamEvent) {
    if let Ok(line) = serde_json::to_string(ev) {
        println!("{line}");
    }
}

#[must_use]
pub fn empty_report(
    mode: &str,
    role: SlotRole,
    tier: ConfidenceTier,
    workers_requested: usize,
    worker_limit_reason: &str,
    wall_ms: u64,
    discovery_ms: u64,
) -> RunReport {
    RunReport {
        mode: mode.to_owned(),
        role,
        tier,
        workers_requested,
        worker_limit_reason: worker_limit_reason.to_owned(),
        wall_ms,
        discovery_ms,
        pool_ready_ms: 0,
        tia_parse_ms: 0,
        tia_graph_ms: 0,
        collect_total_ms: 0.0,
        db_setup_total_ms: 0.0,
        db_template_hits: 0,
        total_collected: 0,
        tests_selected: 0,
        tests_cached: 0,
        tests_tainted: 0,
        deselected: 0,
        passed: 0,
        failed: 0,
        skipped: 0,
        xfailed: 0,
        xpassed: 0,
        errors: 0,
        timed_out: 0,
        crashed: 0,
        flaky: 0,
        fast_path_tests: 0,
        full_pytest_tests: 0,
        worker_restarts: 0,
        coverage_file: None,
        collection_errors: Vec::new(),
        verdict: ExecVerdict::Pass,
        exit_code: 0,
        worker_stats: Vec::new(),
        results: Vec::new(),
        traces: Vec::new(),
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
