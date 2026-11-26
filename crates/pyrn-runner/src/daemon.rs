//! Persistent Warm Worker Pool Daemon (`pyrn daemon start|stop|status`, `pyrn run --daemon`).
//!
//! Keeps pre-booted, affinity-partitioned Python workers alive across runs, segregated by
//! `(SlotRole, project_fp, runtime_fp)`:
//! - Zero Python startup, zero `django.setup()`, zero `conftest.py` reload on warm runs.
//! - Content-hashed BLAKE3 `core_epoch` detection: if `src/`, `conftest.py`, `pyproject.toml`,
//!   `.pyrn.toml`, or migrations change, workers are transparently recycled.
//! - If only test files change (including same-second, same-size edits OR deletions), broadcasts
//!   `WorkerCommand::Invalidate` to EVERY worker (busy or idle) before dispatch.
//! - Supports `--trace`, `--cov-sysmon`, and unified `--tia --cache` explicit nodeid subsets.

use crate::discovery::{
    RepoFingerprint, compute_migrations_hash, diff_changed_test_files, discover_test_files,
    scan_repo_fingerprint,
};
use crate::isolation::{
    CacheLayout, generate_auth_token, merge_coverage_shards, short_sandbox_root,
};
use crate::protocol::{
    ConfidenceTier, DaemonRequest, DaemonResponse, ResourceLane, RunReport, SlotRole,
    WorkerCommand, WorkerEvent,
};
use crate::resources::detect_worker_budget;
use crate::scheduler::{AffinityWorkQueue, TimingHistory};
use crate::supervisor::{
    LiveWorker, RunConfig, WorkerSpawnSpec, empty_report, execute_on_pool, find_conftest_seed_dir,
    spawn_worker, spawn_worker_pool,
};
use anyhow::{Context, Result, anyhow, bail};
use pyrn_core::config::PyrnConfig;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonStateFile {
    pub pid: u32,
    pub host: String,
    pub port: u16,
    pub auth_token: String,
    pub project_root: String,
    pub role: SlotRole,
    pub workers: usize,
}

#[derive(Clone, Debug)]
pub struct DaemonServeConfig {
    pub project_root: PathBuf,
    pub python_exe: String,
    pub config_file: Option<PathBuf>,
    pub markers: Option<String>,
    /// `None` uses container-aware physical-core auto-sizing (`-w auto`).
    pub workers: Option<usize>,
    pub role: SlotRole,
    pub cache_dir: Option<PathBuf>,
    pub sqlite_template: bool,
    pub extra_pytest_args: Vec<String>,
}

/// Start the warm persistent daemon server (runs until `daemon stop` or Ctrl-C).
pub async fn serve_daemon(cfg: DaemonServeConfig) -> Result<()> {
    let mut pyrn_cfg = PyrnConfig::load_from_dir(&cfg.project_root);
    let migrations_hash = compute_migrations_hash(&cfg.project_root, &cfg.python_exe);
    let cache = CacheLayout::prepare(
        cfg.cache_dir.as_deref(),
        cfg.role,
        &cfg.project_root,
        &cfg.python_exe,
        &migrations_hash,
    )?;

    let budget = detect_worker_budget(Some(&cfg.project_root), cfg.workers, None);
    let w = budget.selected_workers.max(1);
    let sqlite_w = if w == 1 { 1 } else { ((w * 2) / 5).max(1) };
    let herm_w = w - sqlite_w;
    let mut lanes = Vec::with_capacity(w);
    for _ in 0..sqlite_w {
        lanes.push(ResourceLane::SqliteDjango);
    }
    for _ in 0..herm_w {
        lanes.push(ResourceLane::Hermetic);
    }

    let worker_listener = TcpListener::bind("127.0.0.1:0").await?;
    let worker_port = worker_listener.local_addr()?.port();
    let worker_listener = Arc::new(Mutex::new(worker_listener));
    let worker_token = generate_auth_token();

    let conftest_seed_dir = {
        let t = cfg.project_root.join("tests");
        t.join("conftest.py").is_file().then_some(t)
    };
    let config_file = cfg.config_file.clone().or_else(|| {
        let p = cfg.project_root.join("pyproject.toml");
        p.is_file().then_some(p)
    });

    let mut spawn_spec = WorkerSpawnSpec {
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
        cov_shard_dir: None,
        cov_sysmon: true, // Always expose COVERAGE_CORE=sysmon so per-run cov requests work zero-restart!
        pure_autouse_guards: pyrn_cfg.pure_autouse_guards.clone(),
        extra_pytest_args: cfg.extra_pytest_args.clone(),
        host: "127.0.0.1".to_owned(),
        port: worker_port,
        auth_token: worker_token,
    };

    let t_boot = Instant::now();
    let mut pool = spawn_worker_pool(&spawn_spec, &worker_listener, &lanes).await?;
    let mut repo_fp: RepoFingerprint = scan_repo_fingerprint(&cfg.project_root, &pyrn_cfg);
    let mut current_migrations_hash = migrations_hash;
    let mut history = TimingHistory::load(&cache.timings_path);
    let mut runs_served = 0u64;

    let rpc_listener = TcpListener::bind("127.0.0.1:0").await?;
    let rpc_port = rpc_listener.local_addr()?.port();
    let rpc_token = generate_auth_token();

    let state = DaemonStateFile {
        pid: std::process::id(),
        host: "127.0.0.1".to_owned(),
        port: rpc_port,
        auth_token: rpc_token.clone(),
        project_root: cfg.project_root.to_string_lossy().into_owned(),
        role: cfg.role,
        workers: pool.len(),
    };
    write_daemon_state(&cache.daemon_state_path, &state)?;

    eprintln!(
        "[pyrn daemon] READY pid={} role={} {} (hermetic={}, sqlite={}) port={} boot={}ms state={}",
        state.pid,
        cfg.role.as_str(),
        budget.explain(),
        herm_w,
        sqlite_w,
        rpc_port,
        t_boot.elapsed().as_millis(),
        cache.daemon_state_path.display()
    );

    loop {
        let (mut stream, _) = tokio::select! {
            res = rpc_listener.accept() => res?,
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        };
        let _ = stream.set_nodelay(true);
        let (r_half, mut w_half) = stream.split();
        let mut reader = BufReader::new(r_half);
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            continue;
        }

        let req: DaemonRequest = match serde_json::from_str(line.trim_end()) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_daemon_resp(
                    &mut w_half,
                    &DaemonResponse::Error {
                        message: format!("invalid request: {e}"),
                    },
                )
                .await;
                continue;
            }
        };

        match req {
            DaemonRequest::Status { auth_token } => {
                if auth_token != rpc_token {
                    let _ = write_daemon_resp(
                        &mut w_half,
                        &DaemonResponse::Error {
                            message: "unauthorized".to_owned(),
                        },
                    )
                    .await;
                    continue;
                }
                let h_cnt = pool
                    .iter()
                    .filter(|w| w.lane == ResourceLane::Hermetic)
                    .count();
                let s_cnt = pool
                    .iter()
                    .filter(|w| w.lane == ResourceLane::SqliteDjango)
                    .count();
                let _ = write_daemon_resp(
                    &mut w_half,
                    &DaemonResponse::Status {
                        pid: std::process::id(),
                        project_root: cfg.project_root.to_string_lossy().into_owned(),
                        role: cfg.role,
                        workers_total: pool.len(),
                        workers_hermetic: h_cnt,
                        workers_sqlite: s_cnt,
                        code_epoch: repo_fp.core_epoch.clone(),
                        migrations_hash: current_migrations_hash.clone(),
                        runs_served,
                    },
                )
                .await;
            }
            DaemonRequest::Stop { auth_token } => {
                if auth_token != rpc_token {
                    let _ = write_daemon_resp(
                        &mut w_half,
                        &DaemonResponse::Error {
                            message: "unauthorized".to_owned(),
                        },
                    )
                    .await;
                    continue;
                }
                shutdown_pool_parallel(&mut pool).await;
                let _ = fs::remove_file(&cache.daemon_state_path);
                let _ = write_daemon_resp(&mut w_half, &DaemonResponse::Stopped).await;
                break;
            }
            DaemonRequest::Run {
                auth_token,
                paths,
                markers,
                keyword,
                test_timeout_secs,
                reruns,
                fast_path,
                shard_heavy,
                trace,
                trace_granularity,
                trace_symbols,
                cov_sysmon,
                cov_out,
                explicit_nodeids_by_file,
                exclude_nodeids_by_file,
            } => {
                if auth_token != rpc_token {
                    let _ = write_daemon_resp(
                        &mut w_half,
                        &DaemonResponse::Error {
                            message: "unauthorized".to_owned(),
                        },
                    )
                    .await;
                    continue;
                }

                let t_run = Instant::now();
                let t_disc = Instant::now();
                pyrn_cfg = PyrnConfig::load_from_dir(&cfg.project_root);
                let new_fp = scan_repo_fingerprint(&cfg.project_root, &pyrn_cfg);
                let mut changed_tests = Vec::new();
                let mut pool_ready_ms = 0u64;

                // Soundness check: recycle entire warm pool if core code / conftest / settings changed!
                if new_fp.core_epoch != repo_fp.core_epoch || pool.len() < lanes.len() {
                    let t_rec = Instant::now();
                    shutdown_pool_parallel(&mut pool).await;
                    spawn_spec.pure_autouse_guards = pyrn_cfg.pure_autouse_guards.clone();
                    let new_mig = compute_migrations_hash(&cfg.project_root, &cfg.python_exe);
                    if new_mig != current_migrations_hash {
                        current_migrations_hash = new_mig;
                        let new_cache = CacheLayout::prepare(
                            cfg.cache_dir.as_deref(),
                            cfg.role,
                            &cfg.project_root,
                            &cfg.python_exe,
                            &current_migrations_hash,
                        )?;
                        spawn_spec.sqlite_template_path = cfg
                            .sqlite_template
                            .then_some(new_cache.sqlite_template_path);
                    }
                    pool = spawn_worker_pool(&spawn_spec, &worker_listener, &lanes).await?;
                    repo_fp = new_fp;
                    pool_ready_ms = t_rec.elapsed().as_millis() as u64;
                } else {
                    changed_tests = diff_changed_test_files(&repo_fp, &new_fp);
                    repo_fp = new_fp;
                    if !changed_tests.is_empty() {
                        pool = broadcast_invalidate_all(
                            pool,
                            &changed_tests,
                            &spawn_spec,
                            &worker_listener,
                        )
                        .await?;
                        changed_tests.clear();
                    }
                }

                let files =
                    discover_test_files(&cfg.project_root, &paths, markers.as_deref(), &pyrn_cfg)?;
                let discovery_ms = t_disc.elapsed().as_millis() as u64;

                if files.is_empty() {
                    let rep = empty_report(
                        "warm-daemon",
                        cfg.role,
                        ConfidenceTier::StrictImpacted,
                        pool.len(),
                        &budget.limiting_factor,
                        t_run.elapsed().as_millis() as u64,
                        discovery_ms,
                    );
                    let _ = write_daemon_resp(
                        &mut w_half,
                        &DaemonResponse::RunResult {
                            report: Box::new(rep),
                        },
                    )
                    .await;
                    continue;
                }

                let (queue, _, _) = AffinityWorkQueue::build(
                    &files,
                    &history,
                    pool.len(),
                    shard_heavy,
                    explicit_nodeids_by_file.as_deref(),
                    exclude_nodeids_by_file.as_deref(),
                );

                if queue.remaining() == 0 {
                    let rep = empty_report(
                        "warm-daemon",
                        cfg.role,
                        ConfidenceTier::StrictImpacted,
                        pool.len(),
                        &budget.limiting_factor,
                        t_run.elapsed().as_millis() as u64,
                        discovery_ms,
                    );
                    let _ = write_daemon_resp(
                        &mut w_half,
                        &DaemonResponse::RunResult {
                            report: Box::new(rep),
                        },
                    )
                    .await;
                    continue;
                }

                let cov_shard_dir = if cov_sysmon {
                    let d = short_sandbox_root().join(format!(
                        "dcov-{}-{}",
                        std::process::id(),
                        &generate_auth_token()[..8]
                    ));
                    fs::create_dir_all(&d)?;
                    Some(d)
                } else {
                    None
                };

                let run_cfg = RunConfig {
                    project_root: cfg.project_root.clone(),
                    python_exe: cfg.python_exe.clone(),
                    targets: paths,
                    markers,
                    keyword,
                    config_file: cfg.config_file.clone(),
                    workers: Some(pool.len()),
                    role: cfg.role,
                    tier: ConfidenceTier::StrictImpacted,
                    cache_dir: cfg.cache_dir.clone(),
                    test_timeout_secs,
                    reruns,
                    fast_path,
                    shard_heavy,
                    sqlite_template: cfg.sqlite_template,
                    ndjson: false,
                    trace,
                    trace_granularity,
                    trace_symbols,
                    cov_sysmon,
                    cov_out: cov_out.map(PathBuf::from),
                    extra_pytest_args: cfg.extra_pytest_args.clone(),
                    explicit_nodeids_by_file: explicit_nodeids_by_file.map(|b| *b),
                };

                if spawn_spec.conftest_seed_dir.is_none() {
                    spawn_spec.conftest_seed_dir =
                        find_conftest_seed_dir(&cfg.project_root, &files);
                }

                let taken_pool: Vec<LiveWorker> = std::mem::take(&mut pool);
                let (mut report, surviving) = execute_on_pool(
                    taken_pool,
                    queue,
                    &spawn_spec,
                    &worker_listener,
                    &run_cfg,
                    &changed_tests,
                    "warm-daemon",
                    &budget.limiting_factor,
                    discovery_ms,
                    pool_ready_ms,
                    t_run,
                    cov_shard_dir.as_deref(),
                )
                .await?;
                pool = surviving;

                if let Some(ref shard_dir) = cov_shard_dir {
                    let target_cov = run_cfg
                        .cov_out
                        .clone()
                        .unwrap_or_else(|| cache.default_cov_out_path.clone());
                    if let Ok(Some(merged)) = merge_coverage_shards(
                        &cfg.python_exe,
                        &cfg.project_root,
                        shard_dir,
                        &target_cov,
                    )
                    .await
                    {
                        report.coverage_file = Some(merged.to_string_lossy().into_owned());
                    } else {
                        let _ = fs::remove_dir_all(shard_dir);
                    }
                }

                report.wall_ms = t_run.elapsed().as_millis() as u64;
                history.update_and_save(&cache.timings_path, &report.results);
                runs_served += 1;

                let _ = write_daemon_resp(
                    &mut w_half,
                    &DaemonResponse::RunResult {
                        report: Box::new(report),
                    },
                )
                .await;
            }
        }
    }

    shutdown_pool_parallel(&mut pool).await;
    let _ = fs::remove_file(&cache.daemon_state_path);
    Ok(())
}

async fn shutdown_pool_parallel(pool: &mut Vec<LiveWorker>) {
    let mut handles = Vec::with_capacity(pool.len());
    for w in pool.drain(..) {
        handles.push(tokio::spawn(async move {
            w.graceful_shutdown().await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

async fn broadcast_invalidate_all(
    pool: Vec<LiveWorker>,
    changed_paths: &[String],
    spawn_spec: &WorkerSpawnSpec,
    listener: &Arc<Mutex<TcpListener>>,
) -> Result<Vec<LiveWorker>> {
    let mut handles = Vec::with_capacity(pool.len());
    for mut w in pool {
        let changed = changed_paths.to_vec();
        let spec = spawn_spec.clone();
        let lst = Arc::clone(listener);
        handles.push(tokio::spawn(async move {
            for p in &changed {
                w.warm_files.remove(p);
            }
            let cmd = WorkerCommand::Invalidate {
                changed_paths: changed,
            };
            let ok = if w.send_cmd(&cmd).await.is_ok() {
                matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(10), w.read_event()).await,
                    Ok(Ok(Some(WorkerEvent::Invalidated { .. })))
                )
            } else {
                false
            };
            if ok {
                Ok(w)
            } else {
                let slot = w.slot_index;
                let lane = w.lane;
                let restarts = w.restarts + 1;
                w.hard_kill().await;
                spawn_worker(&spec, &lst, slot, lane, restarts).await
            }
        }));
    }
    let mut refreshed = Vec::with_capacity(handles.len());
    for h in handles {
        let w = h.await.context("invalidate task panicked")??;
        refreshed.push(w);
    }
    Ok(refreshed)
}

/// Client helper: connect to a running daemon for `(role, project_root, runtime_fp)` and execute a request.
pub async fn send_daemon_request(
    state_path: &Path,
    make_req: impl FnOnce(String) -> DaemonRequest,
) -> Result<DaemonResponse> {
    let raw = fs::read_to_string(state_path).with_context(|| {
        format!(
            "no active warm daemon found at {} (start one with `pyrn daemon start`)",
            state_path.display()
        )
    })?;
    let state: DaemonStateFile = serde_json::from_str(&raw)?;
    let mut stream = TcpStream::connect((state.host.as_str(), state.port))
        .await
        .with_context(|| format!("connect to warm daemon on {}:{}", state.host, state.port))?;
    let _ = stream.set_nodelay(true);

    let req = make_req(state.auth_token);
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    let n = reader.read_line(&mut resp_line).await?;
    if n == 0 {
        bail!("warm daemon closed connection unexpectedly");
    }
    let resp: DaemonResponse = serde_json::from_str(resp_line.trim_end())
        .map_err(|e| anyhow!("decode daemon response: {e}"))?;
    Ok(resp)
}

/// Query status of a running warm daemon.
pub async fn daemon_status(
    project_root: &Path,
    python_exe: &str,
    role: SlotRole,
    cache_dir: Option<&Path>,
) -> Result<DaemonResponse> {
    let mig = compute_migrations_hash(project_root, python_exe);
    let cache = CacheLayout::prepare(cache_dir, role, project_root, python_exe, &mig)?;
    send_daemon_request(&cache.daemon_state_path, |auth_token| {
        DaemonRequest::Status { auth_token }
    })
    .await
}

/// Stop a running warm daemon cleanly (waits for parallel worker shutdown).
pub async fn daemon_stop(
    project_root: &Path,
    python_exe: &str,
    role: SlotRole,
    cache_dir: Option<&Path>,
) -> Result<DaemonResponse> {
    let mig = compute_migrations_hash(project_root, python_exe);
    let cache = CacheLayout::prepare(cache_dir, role, project_root, python_exe, &mig)?;
    send_daemon_request(&cache.daemon_state_path, |auth_token| DaemonRequest::Stop {
        auth_token,
    })
    .await
}

/// Run a test request against an already-running warm daemon.
pub async fn execute_via_daemon(cfg: &RunConfig) -> Result<RunReport> {
    execute_via_daemon_with_exclusions(cfg, None).await
}

/// Run a test request against a warm daemon with an optional per-file negative CAS skip-list.
pub async fn execute_via_daemon_with_exclusions(
    cfg: &RunConfig,
    exclude_nodeids_by_file: Option<&std::collections::BTreeMap<String, Vec<String>>>,
) -> Result<RunReport> {
    let migrations_hash = compute_migrations_hash(&cfg.project_root, &cfg.python_exe);
    let cache = CacheLayout::prepare(
        cfg.cache_dir.as_deref(),
        cfg.role,
        &cfg.project_root,
        &cfg.python_exe,
        &migrations_hash,
    )?;

    let resp = send_daemon_request(&cache.daemon_state_path, |auth_token| DaemonRequest::Run {
        auth_token,
        paths: cfg.targets.clone(),
        markers: cfg.markers.clone(),
        keyword: cfg.keyword.clone(),
        test_timeout_secs: cfg.test_timeout_secs,
        reruns: cfg.reruns,
        fast_path: cfg.fast_path,
        shard_heavy: cfg.shard_heavy,
        trace: cfg.trace,
        trace_granularity: cfg.trace_granularity.clone(),
        trace_symbols: cfg.trace_symbols,
        cov_sysmon: cfg.cov_sysmon,
        cov_out: cfg
            .cov_out
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        explicit_nodeids_by_file: cfg.explicit_nodeids_by_file.clone().map(Box::new),
        exclude_nodeids_by_file: exclude_nodeids_by_file.cloned().map(Box::new),
    })
    .await?;

    match resp {
        DaemonResponse::RunResult { mut report } => {
            report.tier = cfg.tier;
            Ok(*report)
        }
        DaemonResponse::Error { message } => bail!("daemon error: {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn write_daemon_state(path: &Path, state: &DaemonStateFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

async fn write_daemon_resp(
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    resp: &DaemonResponse,
) -> Result<()> {
    let mut s = serde_json::to_string(resp)?;
    s.push('\n');
    writer.write_all(s.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}
