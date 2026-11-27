# Benchmarks

All measurements below were collected on an anonymized, real-world **12,001-test Django + Ray
codebase** (`279` test files, `11,946` passed, `0` failed, `55` skipped) running on a
16-physical-core / 32-logical-thread workstation with Python 3.12 and `pytest` 9.x.

## Methodology and parity verification

- **Exact nodeid parity:** every full-suite `pyrn` run was cross-checked against serial vanilla
  `pytest` to verify `12,001/12,001` identical collected test nodeids (`0` missing, `0` extra) and
  identical pass/skip outcomes (`11,946` passed, `0` failed, `55` skipped).
- **Anti-SMT automatic sizing (`-w auto`):** on the 16-core / 32-thread host, `pyrn` automatically
  selected `workers_selected=16 [limit=physical_cpus]` rather than oversubscribing logical SMT
  threads.
- **Read-only historical TIA replay:** Git history recall and speedup were measured with
  `pyrn replay-git --limit 30 --tier strict` over 30 consecutive non-merge commits, feeding only
  non-test source diffs into TIA and measuring whether existing co-changed test files were
  predicted.

## Full-suite execution (12,001 tests)

| Runner configuration | Wall time | Speedup vs serial | Speedup vs `xdist -n 16` | Observed metrics |
| --- | ---: | ---: | ---: | --- |
| Serial `pytest` | `342.19 s` | `1.00×` | — | Baseline (`12,001` nodeids) |
| `pytest-xdist -n 16` | `77.87 s` | `4.39×` | `1.00×` | Redundant collection per worker; shared temp/DB fragility |
| `pyrn run -w auto` (cold #1, empty cache) | `47.83 s` | `7.15×` | `1.63×` | `16` workers, `15` DB template hits, `0` restarts |
| `pyrn run -w auto` (cold #2, warm templates & timings) | `38.27 s` | `8.94×` | `2.03×` | `sum_db_setup=0.05 s`, `26` DB template hits, `484 B` receipt |
| `pyrn run --daemon -w auto` (warm #1) | `33.44 s` | `10.23×` | `2.33×` | `99.6%` worker efficiency, `0.09 s` tail skew |
| `pyrn run --daemon -w auto` (warm #2, steady-state) | `29.50 s` | **`11.60×`** | **`2.64×`** | `99.1%` efficiency, `sum_db_setup=0.02 s`, `491 B` receipt |

In steady-state warm-daemon mode, LPT + heavy-file sharding and sticky work-stealing kept active
worker utilization above `99%` with `0.09 s` tail skew across all 16 workers and zero worker
restarts.

## SQLite migration template cloning

Database-heavy Django tests spend most of their serial runtime repeatedly applying migrations.
`pyrn` clones BLAKE3-keyed migration templates for both `:memory:` (`sqlite3.backup()`) and fresh
file-backed SQLite databases:

| Workload | Serial `pytest` | `pyrn` | Speedup | Parity / notes |
| --- | ---: | ---: | ---: | --- |
| Migration-heavy 83-test unit suite (`31.21 s` DB setup) | `32.45 s` | **`1.65 s`–`1.76 s`** | **`18.4×`–`19.7×`** | `83/83` passed, `0.22 s` setup, `21` template hits |
| Dedicated migration integration suites | — | Clean pass | — | `56/56` passed, `0` regressions |

## Pure-Rust symbol-level TIA (30-commit Git replay)

`pyrn-core` parses and hashes the entire 279-test-file repository in pure Rust (`rustpython-parser`
+ `rayon`) in **~130 ms** (`~104 ms` parse/hash + `~26 ms` dependency graph build).

Across 30 real non-merge commits:

| Metric | File-level baseline | Symbol-level TIA (`strict`) |
| --- | ---: | ---: |
| Co-change recall | `56/56` (`100.0%`) | **`56/56` (`100.0%`)** |
| Mean test reduction | `56.4%` | **`59.4%`** |
| Median node speedup | `7.6×` | **`8.0×`** |
| Median duration-weighted wall speedup | — | **`4.60×`** |

Symbol-level (`def`/`class`) pruning breaks hub-module fanout without losing recall—for example,
narrowing impacted test files on hub-module commits from `234 → 166`, `232 → 105`, and `48 → 16`
files while preserving `100%` co-change recall.

## Unified `--tia --cache` incremental runs

Combining symbol-level TIA with the role-segregated BLAKE3 SQLite WAL cache (`CasTestCache`):

| Scenario | Cold impacted run | Cached run | Speedup vs `342.19 s` serial | Notes |
| --- | ---: | ---: | ---: | --- |
| Narrow module edit (`--tia --cache`) | `6.16 s` | **`1.51 s`** | **`226×`** | `90%` cache hit rate, `473 B` receipt, `0` cross-role bleed on `--role verify` |
| Wide settings edit (`--tia --cache --daemon`) | `28.71 s` | **`11.02 s`** | **`31.1×`** | `92%` cache hit rate (`4,796` hits) |
| 100% cache hit (`DJANGO_SETTINGS_MODULE` keyed) | — | **`0.24 s`** | — | `0` worker processes spawned |

## Resilience and coverage benchmarks

- **Multi-fault stress suite:** under simultaneous per-test timeout, hard worker crash
  (`os._exit(139)`), and transient flake injection with `--reruns`, `pyrn` completed in
  **`5.24 s`** (`6` passed, `1` timed out, `2` crashed, `1` flaky, `5` clean worker restarts) with
  zero lost sibling test results.
- **Built-in `--cov-sysmon`:** using Python 3.12+ `COVERAGE_CORE=sysmon` with isolated per-worker
  shards merged outside the repo tree is typically **2.4×–3.1× faster** than C-trace coverage
  (`ctrace`).

## Reproducing on your own repository

Speedups depend on your suite's collection cost, migration volume, module graph structure, and CPU
count. Measure your own repository directly:

```sh
# 1. Measure historical TIA recall and speedup (read-only)
pyrn replay-git --repo . --limit 30 --tier strict

# 2. Measure cold and warm execution with structured KV metrics
pyrn run --repo . -w auto --metrics-kv
```

## Scope notes and caveats

- Linux cgroup v2/v1 CPU quota, cpuset, and memory OOM guard parsers are verified by unit tests
  against synthetic cgroup fixtures; verify your target container pod directly with
  `pyrn doctor --repo .`.
- PostgreSQL `WITH TEMPLATE` cloning, Linux zygote `fork()` workers, and Win32 Job Object process
  containment are planned post-`v0.1.0` items.
