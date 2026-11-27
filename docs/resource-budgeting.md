# Resource budgeting (`-w auto`)

`pyrn`'s worker budgeter (`crates/pyrn-runner/src/resources.rs`, `#![forbid(unsafe_code)]`) sizes the Python worker pool conservatively to match real physical CPU capacity, container quotas, and container memory headroom—preventing both SMT hyperthreading contention on workstations and Linux OOMKills (`exit 137`) inside Docker, Kubernetes, and GitHub Actions runners.

## Resolution algorithm

When `-w auto` is active (the default), `resolve_worker_budget` starts from **physical CPU cores** (`num_cpus::get_physical()`, ignoring logical SMT sibling threads) and takes the minimum across every active constraint, floored at `1`:

```text
auto_workers = max(1, min(
    physical_cpus,
    available_parallelism,
    cgroup_cpu_quota,          # Linux cgroups v2 cpu.max or v1 cfs_quota_us/cfs_period_us
    cpuset,                    # Linux cgroups v2/v1 cpuset CPU list
    cgroup_memory_oom_guard,   # floor(cgroup_mem_mb * 0.75 / worker_mem_mb)
    env_PYRN_MAX_WORKERS,      # PYRN_MAX_WORKERS env var (if > 0)
    config_max_workers,        # .pyrn.toml / pyproject.toml max_workers (if > 0)
))
```

Finally, for cold runs with a known number of scheduled work units (`scheduled_units > 0`), `pyrn` caps the final count so idle Python interpreters are never spawned:

```text
selected_workers = min(auto_workers, scheduled_units)   # if 0 < scheduled_units < auto_workers
```

When an explicit integer `-w N` (`N >= 1`) is supplied on the CLI, `explicit_cli` bypasses all auto-sizing caps above (still capping at `scheduled_units` in cold runs when `0 < scheduled_units < N`).

## Active constraint probes

| Constraint (`limiting_factor`) | Platform | Probe source | Parsing and clamping rule |
| --- | --- | --- | --- |
| `physical_cpus` | All | `num_cpus::get_physical()` | Baseline anti-SMT cap (`>= 1`). Logical cores (`num_cpus::get()`) are reported for diagnostics only. |
| `available_parallelism` | All | `std::thread::available_parallelism()` | OS scheduler/affinity limit (falls back to `logical_cpus`, `>= 1`). |
| `cgroup_cpu_quota` | Linux | **v2**: walks upward from `/sys/fs/cgroup/<rel>` (via `/proc/self/cgroup` `0::<rel>`) to `/sys/fs/cgroup`, reading `cpu.max`.<br>**v1 fallback**: `/sys/fs/cgroup/cpu/cpu.cfs_quota_us` and `cpu.cfs_period_us`. | v2: `"<quota> <period>"` -> `max(1, quota / period)` (integer floor); `"max"` or `0` = unlimited. Tightest ancestor wins.<br>v1: `quota <= 0` or `period <= 0` = unlimited; else `max(1, quota / period)`. |
| `cpuset` | Linux | **v2**: walks upward reading `cpuset.cpus.effective`, then `cpuset.cpus`.<br>**v1 fallback**: `/sys/fs/cgroup/cpuset/cpuset.cpus`. | Parses comma-separated CPU IDs and inclusive ranges (e.g., `"0-3,8,10-11"` -> `7`), deduplicated via `BTreeSet`. Rejects reversed ranges (`end < start`) or spans `> 16,384`. Tightest ancestor wins. |
| `cgroup_memory_oom_guard` | Linux | **v2**: walks upward reading `memory.max`.<br>**v1 fallback**: `/sys/fs/cgroup/memory/memory.limit_in_bytes`. | Ignores `"max"`, `0`, and unlimited kernel sentinels `> 1 TiB` (`1,099,511,627,776` bytes, e.g., `9223372036854771712`). Converts bytes to MiB (`>= 1`), then computes `max(1, floor((mem_mb * 0.75) / worker_mem_mb))`. |
| `env_PYRN_MAX_WORKERS` | All | `PYRN_MAX_WORKERS` | Positive integer `>= 1` (`0` or non-numeric ignored). |
| `config_max_workers` | All | `.pyrn.toml` / `pyrn.toml` / `[tool.pyrn]` `max_workers` | Positive integer `>= 1` (`0` ignored). |
| `scheduled_units` | All | Number of work units in `AffinityWorkQueue` | Applied after either auto-sizing or explicit `-w N` when `0 < scheduled_units < selected`. |

!!! note "Linux-only cgroup probing"
    Cgroup CPU, cpuset, and memory files are probed when compiled for `target_os = "linux"`. On Windows and macOS, those three fields are `None` and `-w auto` resolves against `physical_cpus`, `available_parallelism`, `PYRN_MAX_WORKERS`, `config_max_workers`, and `scheduled_units`.

## Override precedence

### Worker count precedence

1. **CLI `-w <N>` / `--workers <N>`**: sets `limiting_factor = "explicit_cli"` and overrides `physical_cpus`, cgroups, `PYRN_MAX_WORKERS`, and `config_max_workers`. Still capped by `scheduled_units` during cold runs.
2. **Auto-sizing (`-w auto`, default)**: evaluates constraints in deterministic order (`physical_cpus` -> `available_parallelism` -> `cgroup_cpu_quota` -> `cpuset` -> `cgroup_memory_oom_guard` -> `env_PYRN_MAX_WORKERS` -> `config_max_workers`), tightening strictly on `<` (ties keep the earlier constraint as `limiting_factor`), then applies `scheduled_units`.

### Per-worker memory estimate (`worker_mem_mb`) precedence

Used by the `cgroup_memory_oom_guard` formula (`75%` safety headroom):

1. `PYRN_WORKER_MEM_MB` environment variable (positive integer MiB).
2. `worker_mem_mb` in `.pyrn.toml` / `pyrn.toml` / `[tool.pyrn]` (top-level or `[runner]`).
3. Built-in default: `DEFAULT_WORKER_MEM_MB = 300` MiB.

Increase `worker_mem_mb` (for example, `512` or `768`) for test suites with large ML/data-science dependencies so the 75% headroom formula stays safely below container OOM limits.

## Worked examples

| Environment | Host / Container inputs | `selected_workers` | `limiting_factor` |
| --- | --- | --- | --- |
| 16C / 32T workstation | `physical=16`, `logical=32`, `avail=32`, `units=300` | `16` | `physical_cpus` |
| 64-core Kubernetes node, 4 CPU / 4 GiB pod | `physical=64`, `cpu.max="400000 100000"` (`4`), `cpuset="0-7"` (`8`), `memory.max=4096 MiB` (cap `10`), `units=200` | `4` | `cgroup_cpu_quota` |
| 64-core Kubernetes node, 16 CPU / 1.5 GiB pod | `physical=64`, `cgroup_cpu=16`, `memory.max=1536 MiB`, `worker_mem_mb=300` (`floor(1536 * 0.75 / 300) = 3`), `units=100` | `3` | `cgroup_memory_oom_guard` |
| Fractional CPU pod (`1.5` CPU) | `physical=32`, `cpu.max="150000 100000"` (`1`), `cpuset="0-3,8-9"` (`6`), `units=20` | `1` | `cgroup_cpu_quota` |
| Small TIA run on 16C host | `physical=16`, `units=2` | `2` | `scheduled_units` |

## Inspecting budgets with `pyrn doctor`

Run `pyrn doctor --repo .` to inspect the resolved budget and every detected probe before running tests:

```text
=== pyrn doctor (/workspace/myservice) ===
  Worker budget: workers=4 (limit=cgroup_cpu_quota | physical=64 logical=128 avail=64 cgroup_cpu=4 cpuset=8 cgroup_mem=4096MiB->cap=10 (@300MiB/w))
  Python env:    /workspace/myservice/.venv/bin/python (Python=3.12.8, pytest=9.0.2, coverage=7.8.0, sys.monitoring=true)
  Discovery:     testpaths=["tests"] python_files=["test_*.py", "*_test.py"] pure_autouse_guards=[]
  Cache / DB:    role=agent dir=/tmp/pyrn-cache/agent/... cas_db=/tmp/pyrn-cache/agent/.../cas_cache.sqlite3 sqlite_template_ready=true
  Warm daemon:   stopped (start with `pyrn daemon start --repo .`)
```

Pass `--json` to inspect the full structured `ResourceBudget` object (`selected_workers`, `limiting_factor`, `physical_cpus`, `logical_cpus`, `available_parallelism`, `cgroup_cpu_limit`, `cpuset_limit`, `cgroup_mem_limit_mb`, `worker_mem_mb`, `mem_worker_cap`, `env_cap`, `config_cap`, `scheduled_units`). During `pyrn run`, the selected worker count is also emitted in `--metrics-kv` as `workers_selected=<N>` and in the run summary header as `workers=<N> [limit=<limiting_factor>]`.

## Related guides

- [Configuration](configuration.md) — configuring `max_workers`, `worker_mem_mb`, `PYRN_MAX_WORKERS`, and `PYRN_WORKER_MEM_MB`.
- [CI adoption](ci-adoption.md) — sizing runners on GitHub Actions, Docker, and Kubernetes.
- [Troubleshooting](troubleshooting.md) — diagnosing container OOMs and worker count mismatches.
