//! Pure-Rust, container-aware worker resource budgeting (`#![forbid(unsafe_code)]`).
//!
//! Computes [`ResourceBudget`] (alias [`EffectiveWorkerBudget`]) as the minimum of all active
//! host and container constraints rather than blindly trusting logical SMT thread counts:
//! 1. Physical CPU cores (`num_cpus::get_physical()`, avoiding SMT hyperthreading contention).
//! 2. OS / scheduler affinity (`std::thread::available_parallelism()`).
//! 3. Linux cgroups v2 CPU quota (`cpu.max`) and `cpuset` (`cpuset.cpus.effective` / `cpuset.cpus`).
//! 4. Linux cgroups v1 CPU quota (`cpu.cfs_quota_us` / `cpu.cfs_period_us`) and `cpuset`.
//! 5. Memory OOMKill guard (`memory.max` / `memory.limit_in_bytes` -> `floor(mem_mb * 0.75 / worker_mem_mb)`).
//! 6. Explicit overrides (`PYRN_MAX_WORKERS`, `.pyrn.toml` `max_workers`, CLI `-w N`) and scheduled work units.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// Default estimated resident memory per warm Python/Django worker (MiB).
pub const DEFAULT_WORKER_MEM_MB: u64 = 300;

/// Fraction of container memory budget safe to allocate to test workers before OOM risk.
const MEMORY_SAFETY_HEADROOM: f64 = 0.75;

/// Ignore synthetic unlimited cgroup memory sentinels above 1 TiB (e.g. `9223372036854771712`).
const MAX_REASONABLE_CGROUP_MEM_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

/// Raw host, container, environment, and CLI constraints fed into [`resolve_worker_budget`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConstraints {
    pub physical_cpus: usize,
    pub logical_cpus: usize,
    pub available_parallelism: usize,
    pub cgroup_cpu_limit: Option<usize>,
    pub cpuset_limit: Option<usize>,
    pub cgroup_mem_limit_mb: Option<u64>,
    pub env_max_workers: Option<usize>,
    pub config_max_workers: Option<usize>,
    pub explicit_workers: Option<usize>,
    pub worker_mem_mb: u64,
    pub scheduled_units: Option<usize>,
}

/// Explainable resolved worker budget returned for startup banners, `pyrn doctor`, and KV metrics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceBudget {
    pub selected_workers: usize,
    pub limiting_factor: String,
    pub physical_cpus: usize,
    pub logical_cpus: usize,
    pub available_parallelism: usize,
    pub cgroup_cpu_limit: Option<usize>,
    pub cpuset_limit: Option<usize>,
    pub cgroup_mem_limit_mb: Option<u64>,
    pub worker_mem_mb: u64,
    pub mem_worker_cap: Option<usize>,
    pub env_cap: Option<usize>,
    pub config_cap: Option<usize>,
    pub scheduled_units: Option<usize>,
}

/// Alias matching the architecture spec (`EffectiveWorkerBudget`).
pub type EffectiveWorkerBudget = ResourceBudget;

impl ResourceBudget {
    /// Human-readable one-line explanation of why `-w auto` (or explicit `-w`) selected `N` workers.
    #[must_use]
    pub fn explain(&self) -> String {
        let mut parts = vec![
            format!("physical={}", self.physical_cpus),
            format!("logical={}", self.logical_cpus),
            format!("avail={}", self.available_parallelism),
        ];
        if let Some(c) = self.cgroup_cpu_limit {
            parts.push(format!("cgroup_cpu={c}"));
        }
        if let Some(cs) = self.cpuset_limit {
            parts.push(format!("cpuset={cs}"));
        }
        if let (Some(mem), Some(cap)) = (self.cgroup_mem_limit_mb, self.mem_worker_cap) {
            parts.push(format!(
                "cgroup_mem={}MiB->cap={} (@{}MiB/w)",
                mem, cap, self.worker_mem_mb
            ));
        }
        if let Some(ec) = self.env_cap {
            parts.push(format!("env_max={ec}"));
        }
        if let Some(cc) = self.config_cap {
            parts.push(format!("cfg_max={cc}"));
        }
        if let Some(su) = self.scheduled_units {
            parts.push(format!("units={su}"));
        }
        format!(
            "workers={} (limit={} | {})",
            self.selected_workers,
            self.limiting_factor,
            parts.join(" ")
        )
    }
}

/// Runner-specific budget settings optionally declared in `.pyrn.toml` / `pyproject.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunnerResourceConfig {
    pub max_workers: Option<usize>,
    pub worker_mem_mb: Option<u64>,
}

/// Load optional `max_workers` and `worker_mem_mb` from `pyproject.toml` (`[tool.pyrn]`)
/// and `.pyrn.toml` / `pyrn.toml` via `pyrn_core::config::PyrnConfig`.
#[must_use]
pub fn load_runner_resource_config(project_root: &Path) -> RunnerResourceConfig {
    let cfg = pyrn_core::config::PyrnConfig::load_from_dir(project_root);
    RunnerResourceConfig {
        max_workers: cfg.max_workers.filter(|&n| n > 0),
        worker_mem_mb: cfg.worker_mem_mb.filter(|&m| m > 0),
    }
}

/// Detect active host + container resource constraints and resolve an [`EffectiveWorkerBudget`].
#[must_use]
pub fn detect_worker_budget(
    project_root: Option<&Path>,
    explicit_workers: Option<usize>,
    scheduled_units: Option<usize>,
) -> ResourceBudget {
    let cfg = project_root
        .map(load_runner_resource_config)
        .unwrap_or_default();
    let constraints = detect_host_constraints(
        explicit_workers,
        cfg.max_workers,
        cfg.worker_mem_mb,
        scheduled_units,
    );
    resolve_worker_budget(&constraints)
}

/// Convenience helper returning the resolved auto worker count (e.g. physical cores on desktop,
/// cgroup CPU/memory min inside containers).
#[must_use]
pub fn physical_default_workers(
    project_root: Option<&Path>,
    scheduled_units: Option<usize>,
) -> usize {
    detect_worker_budget(project_root, None, scheduled_units).selected_workers
}

/// Probe host CPU counts, scheduler affinity, Linux cgroups v2/v1 quotas, and env overrides.
#[must_use]
pub fn detect_host_constraints(
    explicit_workers: Option<usize>,
    config_max_workers: Option<usize>,
    config_worker_mem_mb: Option<u64>,
    scheduled_units: Option<usize>,
) -> HostConstraints {
    let physical_cpus = num_cpus::get_physical().max(1);
    let logical_cpus = num_cpus::get().max(1);
    let available_parallelism = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(logical_cpus)
        .max(1);

    let env_max_workers = std::env::var("PYRN_MAX_WORKERS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0);

    let env_worker_mem_mb = std::env::var("PYRN_WORKER_MEM_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&m| m > 0);

    let worker_mem_mb = env_worker_mem_mb
        .or(config_worker_mem_mb)
        .unwrap_or(DEFAULT_WORKER_MEM_MB)
        .max(1);

    let (cgroup_cpu_limit, cpuset_limit, cgroup_mem_limit_mb) = detect_linux_cgroup_limits();

    HostConstraints {
        physical_cpus,
        logical_cpus,
        available_parallelism,
        cgroup_cpu_limit,
        cpuset_limit,
        cgroup_mem_limit_mb,
        env_max_workers,
        config_max_workers,
        explicit_workers: explicit_workers.filter(|&n| n > 0),
        worker_mem_mb,
        scheduled_units,
    }
}

/// Pure, deterministic solver computing [`ResourceBudget`] from [`HostConstraints`].
#[must_use]
pub fn resolve_worker_budget(c: &HostConstraints) -> ResourceBudget {
    let phys = c.physical_cpus.max(1);
    let logi = c.logical_cpus.max(1);
    let avail = c.available_parallelism.max(1);
    let worker_mem_mb = c.worker_mem_mb.max(1);

    let mem_worker_cap = c.cgroup_mem_limit_mb.map(|mem_mb| {
        let usable = (mem_mb as f64) * MEMORY_SAFETY_HEADROOM;
        let workers = (usable / (worker_mem_mb as f64)).floor();
        if workers < 1.0 { 1 } else { workers as usize }
    });

    let (mut selected, mut limiting_factor) = if let Some(explicit) = c.explicit_workers {
        (explicit.max(1), "explicit_cli".to_owned())
    } else {
        // Start from physical cores (anti-SMT) and tighten against every active constraint.
        let mut best = phys;
        let mut reason = "physical_cpus".to_owned();

        let candidates: [(&str, Option<usize>); 6] = [
            ("available_parallelism", Some(avail)),
            ("cgroup_cpu_quota", c.cgroup_cpu_limit),
            ("cpuset", c.cpuset_limit),
            ("cgroup_memory_oom_guard", mem_worker_cap),
            ("env_PYRN_MAX_WORKERS", c.env_max_workers),
            ("config_max_workers", c.config_max_workers),
        ];

        for (label, maybe_cap) in candidates {
            if let Some(cap) = maybe_cap {
                let cap_norm = cap.max(1);
                if cap_norm < best {
                    best = cap_norm;
                    reason = label.to_owned();
                }
            }
        }
        (best, reason)
    };

    // Never spawn more workers than there are scheduled work units.
    if let Some(units) = c.scheduled_units
        && units > 0
        && units < selected
    {
        selected = units;
        limiting_factor = "scheduled_units".to_owned();
    }

    ResourceBudget {
        selected_workers: selected.max(1),
        limiting_factor,
        physical_cpus: phys,
        logical_cpus: logi,
        available_parallelism: avail,
        cgroup_cpu_limit: c.cgroup_cpu_limit,
        cpuset_limit: c.cpuset_limit,
        cgroup_mem_limit_mb: c.cgroup_mem_limit_mb,
        worker_mem_mb,
        mem_worker_cap,
        env_cap: c.env_max_workers,
        config_cap: c.config_max_workers,
        scheduled_units: c.scheduled_units,
    }
}

/// Parse Linux cgroup v2 `cpu.max` content (e.g. `"400000 100000"` -> `Some(4)`,
/// `"150000 100000"` -> `Some(1)`, `"max 100000"` -> `None`).
#[must_use]
pub fn parse_cgroup_v2_cpu_max(content: &str) -> Option<usize> {
    let mut parts = content.split_whitespace();
    let quota_str = parts.next()?;
    let period_str = parts.next()?;
    if quota_str.eq_ignore_ascii_case("max") {
        return None;
    }
    let quota: u64 = quota_str.parse().ok()?;
    let period: u64 = period_str.parse().ok()?;
    if quota == 0 || period == 0 {
        return None;
    }
    Some(((quota / period) as usize).max(1))
}

/// Parse Linux cgroup v1 `cpu.cfs_quota_us` and `cpu.cfs_period_us`
/// (e.g. `("-1", "100000")` -> `None`, `("400000", "100000")` -> `Some(4)`).
#[must_use]
pub fn parse_cgroup_v1_quota(quota_str: &str, period_str: &str) -> Option<usize> {
    let quota: i64 = quota_str.trim().parse().ok()?;
    let period: i64 = period_str.trim().parse().ok()?;
    if quota <= 0 || period <= 0 {
        return None;
    }
    Some(((quota / period) as usize).max(1))
}

/// Parse Linux `cpuset.cpus` / `cpuset.cpus.effective` CPU list syntax
/// (e.g. `"0-3,8,10-11\n"` -> `Some(6)`).
#[must_use]
pub fn parse_cpuset_list(content: &str) -> Option<usize> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut cpus = BTreeSet::new();
    for segment in trimmed.split(',') {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }
        if let Some((start_s, end_s)) = seg.split_once('-') {
            let start: usize = start_s.trim().parse().ok()?;
            let end: usize = end_s.trim().parse().ok()?;
            if end < start || end - start > 16_384 {
                return None;
            }
            for id in start..=end {
                cpus.insert(id);
            }
        } else {
            let id: usize = seg.parse().ok()?;
            cpus.insert(id);
        }
    }
    if cpus.is_empty() {
        None
    } else {
        Some(cpus.len())
    }
}

/// Parse Linux cgroup v2 `memory.max` or cgroup v1 `memory.limit_in_bytes` into MiB.
/// Returns `None` for `"max"`, zero, invalid text, or huge unlimited sentinels (`> 1 TiB`).
#[must_use]
pub fn parse_cgroup_memory_max(content: &str) -> Option<u64> {
    let trimmed = content.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("max") {
        return None;
    }
    let bytes: u64 = trimmed.parse().ok()?;
    if bytes == 0 || bytes > MAX_REASONABLE_CGROUP_MEM_BYTES {
        return None;
    }
    Some((bytes / (1024 * 1024)).max(1))
}

/// Parse `/proc/self/cgroup` to extract the unified cgroup v2 relative path (`0::<rel>`).
#[must_use]
pub fn parse_proc_self_cgroup_v2_rel(content: &str) -> Option<String> {
    for line in content.lines() {
        let mut cols = line.splitn(3, ':');
        let id = cols.next()?;
        let controllers = cols.next()?;
        let path = cols.next()?;
        if id == "0" && controllers.is_empty() {
            let rel = path.trim().trim_start_matches('/');
            return Some(rel.to_owned());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn detect_linux_cgroup_limits() -> (Option<usize>, Option<usize>, Option<u64>) {
    use std::path::PathBuf;

    let cgroup_root = Path::new("/sys/fs/cgroup");

    // 1. Try cgroup v2 unified hierarchy (walking upward from process cgroup to root).
    let v2_rel = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| parse_proc_self_cgroup_v2_rel(&s));

    let mut candidate_dirs: Vec<PathBuf> = Vec::new();
    if let Some(rel) = v2_rel {
        let mut cur = if rel.is_empty() {
            cgroup_root.to_path_buf()
        } else {
            cgroup_root.join(rel)
        };
        loop {
            candidate_dirs.push(cur.clone());
            if cur == cgroup_root {
                break;
            }
            match cur.parent() {
                Some(p) if p.starts_with(cgroup_root) => cur = p.to_path_buf(),
                _ => break,
            }
        }
    } else {
        candidate_dirs.push(cgroup_root.to_path_buf());
    }

    let mut cpu_limit: Option<usize> = None;
    let mut cpuset_limit: Option<usize> = None;
    let mut mem_limit_mb: Option<u64> = None;

    for dir in &candidate_dirs {
        if let Ok(raw) = std::fs::read_to_string(dir.join("cpu.max"))
            && let Some(lim) = parse_cgroup_v2_cpu_max(&raw)
        {
            cpu_limit = Some(cpu_limit.map_or(lim, |prev| prev.min(lim)));
        }
        for name in ["cpuset.cpus.effective", "cpuset.cpus"] {
            if let Ok(raw) = std::fs::read_to_string(dir.join(name))
                && let Some(lim) = parse_cpuset_list(&raw)
            {
                cpuset_limit = Some(cpuset_limit.map_or(lim, |prev| prev.min(lim)));
                break;
            }
        }
        if let Ok(raw) = std::fs::read_to_string(dir.join("memory.max"))
            && let Some(mb) = parse_cgroup_memory_max(&raw)
        {
            mem_limit_mb = Some(mem_limit_mb.map_or(mb, |prev| prev.min(mb)));
        }
    }

    // 2. Fallback to cgroup v1 hierarchy when v2 files aren't populated.
    if cpu_limit.is_none()
        && let (Ok(q), Ok(p)) = (
            std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us"),
            std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us"),
        )
    {
        cpu_limit = parse_cgroup_v1_quota(&q, &p);
    }
    if cpuset_limit.is_none()
        && let Ok(raw) = std::fs::read_to_string("/sys/fs/cgroup/cpuset/cpuset.cpus")
    {
        cpuset_limit = parse_cpuset_list(&raw);
    }
    if mem_limit_mb.is_none()
        && let Ok(raw) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")
    {
        mem_limit_mb = parse_cgroup_memory_max(&raw);
    }

    (cpu_limit, cpuset_limit, mem_limit_mb)
}

#[cfg(not(target_os = "linux"))]
fn detect_linux_cgroup_limits() -> (Option<usize>, Option<usize>, Option<u64>) {
    (None, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_v2_cpu_max_cases() {
        assert_eq!(parse_cgroup_v2_cpu_max("400000 100000\n"), Some(4));
        assert_eq!(parse_cgroup_v2_cpu_max("150000 100000"), Some(1));
        assert_eq!(parse_cgroup_v2_cpu_max("50000 100000"), Some(1));
        assert_eq!(parse_cgroup_v2_cpu_max("max 100000\n"), None);
        assert_eq!(parse_cgroup_v2_cpu_max("0 100000"), None);
        assert_eq!(parse_cgroup_v2_cpu_max("bad input"), None);
    }

    #[test]
    fn parse_v1_quota_cases() {
        assert_eq!(parse_cgroup_v1_quota("400000\n", "100000\n"), Some(4));
        assert_eq!(parse_cgroup_v1_quota("150000", "100000"), Some(1));
        assert_eq!(parse_cgroup_v1_quota("-1\n", "100000\n"), None);
        assert_eq!(parse_cgroup_v1_quota("100000", "0"), None);
    }

    #[test]
    fn parse_cpuset_list_ranges_and_dedup() {
        assert_eq!(parse_cpuset_list("0-3,8,10-11\n"), Some(7));
        assert_eq!(parse_cpuset_list("0-3,8-9"), Some(6));
        assert_eq!(parse_cpuset_list("0-3,2-5"), Some(6));
        assert_eq!(parse_cpuset_list("7"), Some(1));
        assert_eq!(parse_cpuset_list(""), None);
        assert_eq!(parse_cpuset_list("5-2"), None);
    }

    #[test]
    fn parse_cgroup_memory_max_and_sentinels() {
        assert_eq!(parse_cgroup_memory_max("max\n"), None);
        assert_eq!(parse_cgroup_memory_max("9223372036854771712\n"), None);
        // 4 GiB = 4096 MiB
        assert_eq!(parse_cgroup_memory_max("4294967296\n"), Some(4096));
        // 512 MiB
        assert_eq!(parse_cgroup_memory_max("536870912"), Some(512));
        assert_eq!(parse_cgroup_memory_max("0"), None);
    }

    #[test]
    fn desktop_16c_32t_anti_smt_selects_physical_cores() {
        let c = HostConstraints {
            physical_cpus: 16,
            logical_cpus: 32,
            available_parallelism: 32,
            cgroup_cpu_limit: None,
            cpuset_limit: None,
            cgroup_mem_limit_mb: None,
            env_max_workers: None,
            config_max_workers: None,
            explicit_workers: None,
            worker_mem_mb: DEFAULT_WORKER_MEM_MB,
            scheduled_units: Some(300),
        };
        let b = resolve_worker_budget(&c);
        assert_eq!(b.selected_workers, 16);
        assert_eq!(b.limiting_factor, "physical_cpus");
        assert!(b.explain().contains("workers=16"));
        assert!(b.explain().contains("limit=physical_cpus"));
    }

    #[test]
    fn arc_pod_64_core_host_4cpu_4gib_capped_by_cgroup_cpu() {
        let c = HostConstraints {
            physical_cpus: 64,
            logical_cpus: 128,
            available_parallelism: 64,
            cgroup_cpu_limit: parse_cgroup_v2_cpu_max("400000 100000"),
            cpuset_limit: parse_cpuset_list("0-7"),
            cgroup_mem_limit_mb: parse_cgroup_memory_max("4294967296"), // 4 GiB -> cap 10
            env_max_workers: None,
            config_max_workers: None,
            explicit_workers: None,
            worker_mem_mb: 300,
            scheduled_units: Some(200),
        };
        let b = resolve_worker_budget(&c);
        assert_eq!(b.selected_workers, 4);
        assert_eq!(b.limiting_factor, "cgroup_cpu_quota");
        assert_eq!(b.mem_worker_cap, Some(10));
    }

    #[test]
    fn arc_pod_oom_guard_caps_workers_on_tight_memory() {
        // 64-CPU host, 16-CPU cgroup quota, but ONLY 1.5 GiB RAM (1536 MiB * 0.75 / 300 = 3.84 -> 3)
        let c = HostConstraints {
            physical_cpus: 64,
            logical_cpus: 128,
            available_parallelism: 64,
            cgroup_cpu_limit: Some(16),
            cpuset_limit: None,
            cgroup_mem_limit_mb: Some(1536),
            env_max_workers: None,
            config_max_workers: None,
            explicit_workers: None,
            worker_mem_mb: 300,
            scheduled_units: Some(100),
        };
        let b = resolve_worker_budget(&c);
        assert_eq!(b.mem_worker_cap, Some(3));
        assert_eq!(b.selected_workers, 3);
        assert_eq!(b.limiting_factor, "cgroup_memory_oom_guard");
    }

    #[test]
    fn fractional_cpu_quota_and_cpuset_and_unit_cap() {
        let c = HostConstraints {
            physical_cpus: 32,
            logical_cpus: 64,
            available_parallelism: 32,
            cgroup_cpu_limit: parse_cgroup_v2_cpu_max("150000 100000"),
            cpuset_limit: parse_cpuset_list("0-3,8-9"),
            cgroup_mem_limit_mb: None,
            env_max_workers: None,
            config_max_workers: None,
            explicit_workers: None,
            worker_mem_mb: 300,
            scheduled_units: Some(20),
        };
        let b = resolve_worker_budget(&c);
        assert_eq!(b.cgroup_cpu_limit, Some(1));
        assert_eq!(b.cpuset_limit, Some(6));
        assert_eq!(b.selected_workers, 1);
        assert_eq!(b.limiting_factor, "cgroup_cpu_quota");

        // Scheduled units cap overrides when only 2 files are run on a 16-core host
        let small = HostConstraints {
            cgroup_cpu_limit: None,
            cpuset_limit: None,
            scheduled_units: Some(2),
            ..c
        };
        let b2 = resolve_worker_budget(&small);
        assert_eq!(b2.selected_workers, 2);
        assert_eq!(b2.limiting_factor, "scheduled_units");
    }

    #[test]
    fn proc_self_cgroup_v2_parser() {
        let sample = "0::/kubepods.slice/kubepods-burstable.slice/cri-containerd-abc.scope\n";
        assert_eq!(
            parse_proc_self_cgroup_v2_rel(sample),
            Some("kubepods.slice/kubepods-burstable.slice/cri-containerd-abc.scope".to_owned())
        );
    }
}
