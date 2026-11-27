# Assess and adopt in CI

Use this guide when evaluating `pyrn` for an existing Python repository and CI pipeline. Measure
your current bottlenecks first, verify TIA accuracy in read-only shadow mode, and roll out
selective execution and caching in stages without weakening merge gates.

## 1. Diagnose current suite bottlenecks

Before changing CI jobs, inspect where your suite currently spends wall time:

1. **Environment and worker budget:** run `pyrn doctor --repo .` locally and inside your CI
   container image to confirm Python version, `sys.monitoring` support, and effective cgroup CPU
   and memory limits.
2. **Execution profile:** run `pyrn run --repo . -w auto` once and read the **"Why Was This Run
   Slow?"** section. Pay attention to:
   - high `sum_collect` time (redundant collection across workers),
   - high `setup` percentage or `sum_db_setup` (repeated Django/SQLite migrations),
   - high `tail_skew` (straggler test files not sharded cleanly),
   - low `efficiency` (SMT or container CPU throttling).
3. **Historical TIA accuracy:** run a read-only Git history replay over your recent commits:

```sh
pyrn replay-git --repo . --limit 30 --tier strict
```

Supply a CSV of per-file durations (`file_path,duration_s` or `file,total_ms`) via
`--durations-csv` to estimate real wall-time savings alongside co-change recall:

```sh
pyrn replay-git --repo . --limit 30 --tier strict --durations-csv per_file_timings.csv --json
```

Review `cochange_recall_pct` and `full_fallback_commits` before enabling selective execution in CI.

## 2. Moving from `pytest-xdist` and C-trace coverage

| Problem area | Typical `pytest-xdist` / C-trace setup | `pyrn` behavior |
| --- | --- | --- |
| Worker sizing (`auto`) | Uses logical SMT threads or host CPUs inside containers; often thrashes or OOMKills (`exit 137`) | Computes `min(physical_cpus, available_parallelism, cgroup v2/v1 CPU quota, cpuset, 75% memory OOM guard)` |
| Test collection | Every worker collects the entire test tree up front | Incremental per-unit collection on persistent `pytest.Session` workers, with LPT + heavy-file sharding |
| Django / SQLite DB setup | Repeats migrations per worker or per fixture | BLAKE3-keyed `:memory:` (`sqlite3.backup()`) and file-backed SQLite migration template cloning |
| Worker crashes & hangs | A single segfault (`os._exit(139)`) or hang can abort sibling tests or stall the job | Per-test watchdogs (`--test-timeout`), instant worker respawn, and automatic re-queue of remaining tests |
| Temp dirs & ports | Shared temp prefixes and colliding server ports; Windows `MAX_PATH` failures | Short RAII per-worker sandboxes (`<TEMP>/pr/w<slot>-<hex>`), stripped ambient pytest/xdist env, disjoint 100-port blocks |
| Coverage overhead | `COVERAGE_CORE=ctrace` adds heavy Python interpreter overhead | Built-in `--cov-sysmon` (`COVERAGE_CORE=sysmon` on Python 3.12+) with isolated per-worker shards merged cleanly |
| Incremental runs | Re-runs full suite on small edits | Symbol-level Rust TIA (`--tia`) + role-segregated BLAKE3 SQLite WAL cache (`--cache`) |

## 3. Container and Kubernetes / ARC runner pod safety

Hosted Linux containers, Docker builds, and Kubernetes Actions Runner Controller (ARC) pods often
expose the host's full physical/logical core count (e.g., 64 cores) while capping the container to
2–4 CPUs and a few GiB of RAM. Spawning 32–64 Python workers in that environment causes severe CPU
throttling and kernel OOMKills (`exit 137`).

With `-w auto`, `pyrn` walks the process's Linux cgroup v2 hierarchy (via `/proc/self/cgroup` and
`/sys/fs/cgroup/.../cpu.max`, `cpuset.cpus.effective`, `memory.max`) with cgroup v1 fallback, and
enforces a memory headroom cap of `floor(cgroup_mem_mb * 0.75 / worker_mem_mb)`.

Before enabling CI jobs on a new runner pool:

- Run `pyrn doctor --repo . --json` inside the exact container image and pod spec.
- If your suite loads heavy ML/data libraries (such as Ray, PyTorch, or large fixtures), increase
  `worker_mem_mb` in `.pyrn.toml` (`[runner].worker_mem_mb`, default `300`) or set
  `PYRN_WORKER_MEM_MB`.
- Use `PYRN_MAX_WORKERS` or `[runner].max_workers` if a shared runner reserves capacity for
  sidecar services.

See [Resource budgeting](resource-budgeting.md) for exact formulas and override precedence.

## 4. Three-stage CI rollout

Adopt `pyrn` incrementally so every step is observable and reversible.

### Stage 1: Shadow TIA and baseline full execution

Keep your existing CI gate intact while running `pyrn` in shadow mode to measure plan precision and
execution parity:

1. Check out PR branches with full history (`fetch-depth: 0`) so `git diff` against the base
   branch succeeds locally without fetching.
2. Run read-only planning alongside CI and archive bounded receipts:

```sh
pyrn plan --repo . --base origin/main --tier strict --metrics-kv --receipt-out plan-receipt.json
```

3. Run `pyrn run --repo . -w auto --role verify --metrics-kv --receipt-out run-receipt.json` as an
   additional check and confirm collected test counts match your baseline `pytest` run.

### Stage 2: Fast PR feedback with TIA and role-segregated CAS cache

Once shadow plans show high recall and full-run parity is verified, switch pull-request feedback
jobs to unified TIA + CAS caching:

```sh
pyrn run --repo . \
  --tia --base origin/main \
  --cache --cache-dir .pyrn-cache \
  --role agent \
  --tier strict \
  -w auto \
  --metrics-kv \
  --receipt-out pr-receipt.json
```

- Persist `.pyrn-cache` between workflow runs with your CI cache action (keyed by OS, Python
  version, and lockfile hash), and prune aged entries periodically with
  `pyrn cache prune --repo . --cache-dir .pyrn-cache --max-age-days 30`.
- Keep fail-closed `unknown_path_policy = "full"` (the default) so edits to dependency manifests,
  Dockerfiles, settings, or unmapped assets automatically upgrade the run to `full_verified`.
- Add repository-specific global triggers to `[tia].extra_full_triggers` and custom runtime
  environment prefixes to `[tia].env_allowlist_prefixes` in `.pyrn.toml`.

### Stage 3: Authoritative merge gate with `--role verify`

Reserve `--role verify` for clean merge-queue, `main`, or release-gate workflows. Because `agent`
and `verify` roles maintain completely separate on-disk directories and SQLite primary-key
partitions, interactive developer or PR-agent caches can never satisfy a `verify` run:

```sh
pyrn run --repo . \
  --role verify \
  --tier full \
  -w auto \
  --reruns 1 \
  --test-timeout 60 \
  --metrics-kv \
  --receipt-out verify-receipt.json
```

If you want faster pre-merge verification on large monorepos while keeping conservative file-level
closure, use `--tia --base origin/main --tier transitive --role verify` on PRs and `--tier full
--role verify` on the merge queue or `main`.

Preserve the three-way exit contract in every CI script: `0` passes (`pass` or `flaky`), `1`
blocks on test failures/timeouts/crashes, and `2` alerts on configuration or operational errors.

## Related references

- Copy-paste CI and container configs: [Recipes](recipes.md)
- TIA rules and fallback triggers: [Test Impact Analysis](tia.md)
- Cache keys and role isolation: [Content-addressed cache](caching.md)
- Machine-readable CI outputs: [Receipts and telemetry](receipts-and-telemetry.md)
