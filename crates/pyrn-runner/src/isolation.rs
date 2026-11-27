//! Per-worker isolation: ephemeral RAII temp directories, deterministic port ranges,
//! sterile environment sanitization, `--cov-sysmon` per-worker coverage shards + combine,
//! and `(role, project_fp, runtime_fp)` cache segregation by [`SlotRole`].

use crate::protocol::SlotRole;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

/// Embedded unified Python worker source (keeps the Rust library/binary 100% self-contained).
pub const EMBEDDED_WORKER_PY: &str = include_str!("../python/pyrn_worker.py");

/// Embedded standalone pytest dynamic trace plugin source.
pub const EMBEDDED_TRACE_PLUGIN_PY: &str = include_str!("../python/pytest_pyrn_trace.py");

/// Env vars stripped from every worker to eliminate xdist/pytest/ambient bleed.
const STRIPPED_ENV_KEYS: &[&str] = &[
    "PYTEST_CURRENT_TEST",
    "PYTEST_XDIST_WORKER",
    "PYTEST_XDIST_WORKER_COUNT",
    "PYTEST_XDIST_TESTRUNUID",
    "PYTEST_ADDOPTS",
    "COVERAGE_FILE",
    "COVERAGE_CORE",
    "COV_CORE_SOURCE",
    "COV_CORE_CONFIG",
    "COV_CORE_DATAFILE",
];

/// Cache layout segregated by `(role: agent|verify, project_fp, runtime_fp)`.
#[derive(Clone, Debug)]
pub struct CacheLayout {
    pub project_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub worker_script: PathBuf,
    pub sqlite_template_path: PathBuf,
    pub timings_path: PathBuf,
    pub daemon_state_path: PathBuf,
    pub cas_db_path: PathBuf,
    pub default_cov_out_path: PathBuf,
    pub project_fp: String,
    pub runtime_fp: String,
    pub env_fingerprint: String,
}

/// Strip Windows extended-length verbatim prefix (`\\?\...`) so child
/// processes like Python, Node.js, and Git receive standard native paths.
#[must_use]
pub fn clean_canonical_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && rest.len() >= 2
            && rest.as_bytes()[1] == b':'
        {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Short OS-temp base directory for ephemeral worker `TMPDIR`/`TEMP` sandboxes.
/// Kept short intentionally to avoid Windows 260-char `MAX_PATH` failures when tests
/// create content-addressed SHA-256 filenames inside `pytest`'s `tmp_path`.
#[must_use]
pub fn short_sandbox_root() -> PathBuf {
    clean_canonical_path(std::env::temp_dir()).join("pr")
}

impl CacheLayout {
    pub fn prepare(
        cache_base: Option<&Path>,
        role: SlotRole,
        project_root: &Path,
        python_exe: &str,
        migrations_hash: &str,
    ) -> Result<Self> {
        let base = match cache_base {
            Some(p) => p.to_path_buf(),
            None => clean_canonical_path(std::env::temp_dir()).join("pyrn-cache"),
        };

        let project_canon = clean_canonical_path(
            fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf()),
        );
        let project_fp = blake3::hash(project_canon.to_string_lossy().as_bytes())
            .to_hex()
            .as_str()[..16]
            .to_owned();

        // Runtime fingerprint (OS + ARCH + python_exe) is stable across code/conftest/migration
        // edits so `run --daemon` and `daemon stop` ALWAYS discover the live daemon and let it
        // recycle workers cleanly on `core_epoch` / `migrations_hash` changes.
        let mut rt_h = blake3::Hasher::new();
        rt_h.update(b"pyrn.runtime.v1\0");
        rt_h.update(std::env::consts::OS.as_bytes());
        rt_h.update(b"\0");
        rt_h.update(std::env::consts::ARCH.as_bytes());
        rt_h.update(b"\0");
        rt_h.update(python_exe.as_bytes());
        let runtime_fp = rt_h.finalize().to_hex().to_string();
        let runtime_short = &runtime_fp[..16];

        let mut env_h = blake3::Hasher::new();
        env_h.update(b"pyrn.env.v1\0");
        env_h.update(runtime_fp.as_bytes());
        env_h.update(b"\0");
        env_h.update(migrations_hash.as_bytes());
        let env_fingerprint = env_h.finalize().to_hex().to_string();

        let project_dir = base.join(role.as_str()).join(&project_fp);
        let runtime_dir = project_dir.join(runtime_short);
        fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("create isolated cache dir {}", runtime_dir.display()))?;

        // Materialize versioned embedded worker script outside target repo.
        let worker_hash_full = blake3::hash(EMBEDDED_WORKER_PY.as_bytes()).to_hex();
        let worker_hash = &worker_hash_full.as_str()[..16];
        let worker_script = runtime_dir.join(format!("pyrn_worker_{worker_hash}.py"));
        if !worker_script.is_file() {
            let tmp = runtime_dir.join(format!(
                "pyrn_worker_{worker_hash}.tmp.{}",
                std::process::id()
            ));
            fs::write(&tmp, EMBEDDED_WORKER_PY)?;
            let _ = fs::rename(&tmp, &worker_script);
        }

        // SQLite template remains strictly keyed by `migrations_hash` so schema edits
        // automatically miss and rebuild a fresh template snapshot!
        let sqlite_template_path = runtime_dir.join(format!(
            "sqlite_template_{}.sqlite3",
            &migrations_hash[..16.min(migrations_hash.len())]
        ));
        let timings_path = project_dir.join("timings.json");
        let daemon_state_path = runtime_dir.join("daemon.json");
        let cas_db_path = project_dir.join("cas_cache.sqlite3");
        let default_cov_out_path = runtime_dir.join("coverage").join(".coverage");

        Ok(Self {
            project_dir,
            runtime_dir,
            worker_script,
            sqlite_template_path,
            timings_path,
            daemon_state_path,
            cas_db_path,
            default_cov_out_path,
            project_fp,
            runtime_fp,
            env_fingerprint,
        })
    }
}

/// Ephemeral isolated workspace directory for one worker process (RAII-cleaned on drop).
#[derive(Debug)]
pub struct WorkerSandbox {
    pub tmp_dir: PathBuf,
    pub port_start: u16,
    pub port_end: u16,
    pub worker_id: String,
    pub cov_data_file: Option<PathBuf>,
}

impl WorkerSandbox {
    pub fn create(
        parent: &Path,
        worker_slot: usize,
        worker_id: &str,
        cov_shard_dir: Option<&Path>,
    ) -> Result<Self> {
        let mut rnd = [0u8; 2];
        let _ = getrandom::fill(&mut rnd);
        let hex = format!("{:02x}{:02x}", rnd[0], rnd[1]);
        // Keep path super short (< 48 chars total) for Windows MAX_PATH safety.
        let tmp_dir = parent.join(format!("w{worker_slot}-{hex}"));
        fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("create worker tmp_dir {}", tmp_dir.display()))?;

        // Deterministic disjoint port block of 100 ports per worker slot in [20000..60000).
        let base_port = 20000u16.saturating_add(((worker_slot % 350) as u16) * 100);
        let port_start = base_port;
        let port_end = base_port.saturating_add(99);

        let cov_data_file = if let Some(dir) = cov_shard_dir {
            fs::create_dir_all(dir)?;
            Some(dir.join(format!(".coverage.{worker_id}-{hex}")))
        } else {
            None
        };

        Ok(Self {
            worker_id: worker_id.to_owned(),
            tmp_dir,
            port_start,
            port_end,
            cov_data_file,
        })
    }

    /// Apply isolated environment variables to a Tokio child command.
    pub fn apply_env(&self, cmd: &mut tokio::process::Command, cov_sysmon: bool) {
        for key in STRIPPED_ENV_KEYS {
            cmd.env_remove(key);
        }
        cmd.env("TMPDIR", &self.tmp_dir);
        cmd.env("TEMP", &self.tmp_dir);
        cmd.env("TMP", &self.tmp_dir);
        cmd.env("PYTHONDONTWRITEBYTECODE", "1");
        cmd.env("PYTHONFAULTHANDLER", "1");
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.env("PYTHONUTF8", "1");
        cmd.env("PYRN_WORKER_ID", &self.worker_id);
        cmd.env("PYRN_PORT_START", self.port_start.to_string());
        cmd.env("PYRN_PORT_END", self.port_end.to_string());
        cmd.env(
            "DJANGO_LIVE_TEST_SERVER_ADDRESS",
            format!("127.0.0.1:{}-{}", self.port_start, self.port_end),
        );
        if cov_sysmon {
            cmd.env("COVERAGE_CORE", "sysmon");
            if let Some(ref cov_file) = self.cov_data_file {
                cmd.env("COVERAGE_FILE", cov_file);
            }
        }
    }
}

impl Drop for WorkerSandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.tmp_dir);
    }
}

/// Merge per-worker `.coverage.*` shards cleanly into `target_cov_file` using `<python> -m coverage combine`.
/// Never writes inside `project_root` unless `target_cov_file` explicitly points there.
pub async fn merge_coverage_shards(
    python_exe: &str,
    project_root: &Path,
    shard_dir: &Path,
    target_cov_file: &Path,
) -> Result<Option<PathBuf>> {
    let mut shards = Vec::new();
    if let Ok(entries) = fs::read_dir(shard_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(".coverage."))
            {
                shards.push(p);
            }
        }
    }
    if shards.is_empty() {
        return Ok(None);
    }

    let resolved_target = if target_cov_file.is_absolute() {
        target_cov_file.to_path_buf()
    } else {
        project_root.join(target_cov_file)
    };

    if let Some(parent) = resolved_target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&resolved_target);

    let mut cmd = tokio::process::Command::new(python_exe);
    cmd.arg("-m")
        .arg("coverage")
        .arg("combine")
        .arg("--data-file")
        .arg(&resolved_target);
    for s in &shards {
        cmd.arg(s);
    }
    cmd.current_dir(project_root)
        .env("COVERAGE_CORE", "sysmon")
        .env("COVERAGE_FILE", &resolved_target)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = cmd
        .output()
        .await
        .with_context(|| format!("invoke `{python_exe} -m coverage combine`"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let out = String::from_utf8_lossy(&output.stdout);
        bail!("coverage combine failed: {err} {out}");
    }

    // Clean up ephemeral shard directory after successful merge.
    let _ = fs::remove_dir_all(shard_dir);
    Ok(Some(resolved_target))
}

/// Generate a random 128-bit hex session auth token for loopback TCP handshakes.
#[must_use]
pub fn generate_auth_token() -> String {
    let mut buf = [0u8; 16];
    let _ = getrandom::fill(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_state_path_stable_across_migration_hash_and_segregated_by_role() {
        let base = tempfile::tempdir().expect("tempdir");
        let proj = base.path().join("example-proj");
        let l1 = CacheLayout::prepare(
            Some(base.path()),
            SlotRole::Agent,
            &proj,
            "python",
            "aaaa1111aaaa1111",
        )
        .expect("layout 1");
        let l2 = CacheLayout::prepare(
            Some(base.path()),
            SlotRole::Agent,
            &proj,
            "python",
            "bbbb2222bbbb2222",
        )
        .expect("layout 2");
        let lv = CacheLayout::prepare(
            Some(base.path()),
            SlotRole::Verify,
            &proj,
            "python",
            "aaaa1111aaaa1111",
        )
        .expect("layout verify");

        assert_eq!(l1.daemon_state_path, l2.daemon_state_path);
        assert_ne!(l1.sqlite_template_path, l2.sqlite_template_path);
        assert_ne!(l1.daemon_state_path, lv.daemon_state_path);
        assert_ne!(l1.cas_db_path, lv.cas_db_path);
    }
}
