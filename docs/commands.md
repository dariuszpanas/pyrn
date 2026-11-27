# Command reference

`pyrn` exposes six subcommand groups covering test execution, pure in-memory Test Impact Analysis, warm worker pool lifecycle, content-addressed cache maintenance, environment diagnostics, and read-only Git history replay.

Run `pyrn <command> --help` (or `pyrn <group> <subcommand> --help`) to inspect the exact options supported by your installed binary.

## Command index

| Command | Purpose | Executes tests | Reads Git |
| --- | --- | --- | --- |
| [`pyrn run`](#pyrn-run) | Execute tests in isolated cold or warm-daemon workers with optional `--tia` and `--cache`. | Yes (unless 100% CAS hit) | Only with `--tia`/`--base` (without `--changed`) |
| [`pyrn plan`](#pyrn-plan) | Pure in-memory symbol-level TIA and cache-key planning. | No | Only with `--base` (without `--changed`) |
| [`pyrn daemon start`](#pyrn-daemon-start) | Run a persistent warm Python worker pool in the foreground. | Serves `pyrn run --daemon` | No |
| [`pyrn daemon status`](#pyrn-daemon-status) | Query liveness, worker split, and code/migration epoch of the active daemon. | No | No |
| [`pyrn daemon stop`](#pyrn-daemon-stop) | Gracefully shut down the active warm daemon and its worker processes. | No | No |
| [`pyrn cache status`](#pyrn-cache-status) | Show SQLite WAL CAS cache entry counts, cumulative hits, and disk footprint. | No | No |
| [`pyrn cache prune`](#pyrn-cache-prune) | Remove stale CAS cache entries older than `--max-age-days`. | No | No |
| [`pyrn doctor`](#pyrn-doctor) | Inspect worker resource budget, Python/pytest/coverage/`sys.monitoring`, config, cache, daemon, and slow history. | No | No |
| [`pyrn replay-git`](#pyrn-replay-git) | Replay recent non-merge commits (read-only) to measure TIA recall and reduction. | No | Yes (read-only) |

Global flags accepted on the root command:

| Flag | Short | Effect |
| --- | --- | --- |
| `--help` | `-h` | Print help and exit `0`. |
| `--version` | `-V` | Print package version and exit `0`. |

## Exit codes and stream contract

Every subcommand follows one deterministic exit code contract:

| Exit code | Meaning |
| --- | --- |
| `0` | All selected tests passed (`pass`), every failing test passed on rerun (`flaky`), or an informational/management command succeeded (`plan`, `daemon start`, running `daemon status`, `daemon stop`, `cache status`, `cache prune`, `doctor`, `replay-git`). |
| `1` | Deterministic test failure, setup/teardown/collection error, worker crash, or timeout (`failed` / `timed_out`), or `pyrn daemon status` when no active daemon is running. |
| `2` | Invalid CLI arguments, invalid `--role` / `--tier` / `--workers` value, missing repository path, missing daemon on `daemon stop`, missing native executable in the Python launcher, or any other configuration/operational error. |

Stream conventions:

- **`stdout`**: human-readable summaries, `--json` envelopes, `--ndjson` `StreamEvent` streams, and `--metrics-kv` lines.
- **`stderr`**: operational errors prefixed with `pyrn error: ...`, plus the readiness banner printed by `pyrn daemon start`.
- **Output mode interactions in `pyrn run`**:
  - `--json` prints a single JSON envelope to `stdout` (which includes `metrics_kv`) and suppresses both the human summary and standalone `--metrics-kv` output.
  - `--ndjson` suppresses the human summary; if `--metrics-kv` is also passed (without `--json`), the metrics line is printed after the NDJSON stream.
  - `--receipt-out <PATH>` and `--json-out <PATH>` create parent directories automatically and write files without altering `stdout`.

## Python interpreter resolution

Every subcommand that accepts `--python` resolves the target interpreter in this exact order:

1. Explicit `--python <PATH>` flag or non-empty `PYRN_PYTHON` environment variable.
2. Project-local virtual environments, checked in order:
   - `<repo>/.venv/Scripts/python.exe`
   - `<repo>/.venv/bin/python`
   - `<repo>/venv/Scripts/python.exe`
   - `<repo>/venv/bin/python`
3. Active `$VIRTUAL_ENV/Scripts/python.exe` or `$VIRTUAL_ENV/bin/python`.
4. System fallback: `python` on Windows, `python3` on POSIX.

---

## `pyrn run`

Execute pytest targets across an isolated cold worker pool or a running warm daemon, optionally combining symbol-level TIA (`--tia`), SQLite WAL CAS caching (`--cache`), PEP 669 + PEP 578 + `os.environ` tracing (`--trace`), flake reruns (`--reruns`), and Python 3.12+ `sys.monitoring` coverage (`--cov-sysmon`).

```sh
pyrn run [OPTIONS] [TARGETS_AND_EXTRA]...
```

### Target and pass-through argument parsing

Trailing positional arguments before `--` that do not begin with `-` are treated as target test files or directories. Arguments starting with `-`, and all arguments after `--`, are forwarded as extra pytest arguments:

```sh
pyrn run --repo . tests/unit/test_billing.py -- -vv --maxfail=3
```

### Options

| Flag | Short / Alias | Env var | Default | Description |
| --- | --- | --- | --- | --- |
| `--repo <REPO>` | `--project` | — | `.` | Target repository root directory. |
| `--python <PYTHON>` | — | `PYRN_PYTHON` | auto-detected | Python interpreter executable used by workers. |
| `--tia` | — | — | `false` | Enable symbol-level TIA (`pyrn-core`) to select only impacted test files. |
| `--base <BASE>` | — | — | unset (`HEAD` if `--tia` without `--changed`) | Git base ref for read-only symbol diffing (e.g., `HEAD`, `main`, `origin/main`). Implies `--tia`. |
| `--changed <CHANGED>` | — | — | empty | Explicit changed repo-relative paths (repeatable or comma-separated). Implies `--tia` and takes precedence over `--base`. |
| `--cache` | — | — | `false` | Enable role-segregated SQLite WAL content-addressed test caching (`CasTestCache`). Auto-enables `--trace` on executed files. |
| `--daemon` | — | — | `false` | Route test execution through a running warm daemon for `(role, repo, python)`. |
| `--role <ROLE>` | — | — | `agent` | Cache and daemon isolation role: `agent` or `verify`. |
| `--tier <TIER>` | — | — | `strict` | TIA/cache confidence tier: `strict` (`strict_impacted`), `transitive` (`transitive_impacted`), or `full` (`full_verified`). |
| `--workers <WORKERS>` | `-w` | `PYRN_MAX_WORKERS` (cap under `auto`) | `auto` | Worker count: `auto` (physical cores + cgroup CPU/memory budget) or positive integer `>= 1`. |
| `--cov-sysmon` | — | — | `false` | Capture Python 3.12+ `COVERAGE_CORE=sysmon` coverage into per-worker shards and merge at run end. |
| `--cov-out <COV_OUT>` | — | — | `<cache>/<role>/<proj>/<rt>/coverage/.coverage` | Destination path for merged `--cov-sysmon` coverage file. |
| `--metrics-kv` | — | — | `false` | Emit `<PREFIX> key=<i64> ...` telemetry line to `stdout`. |
| `--metrics-prefix <PREFIX>` | — | `PYRN_METRICS_PREFIX` | `PYRN_METRIC` | Prefix for `--metrics-kv` output. |
| `--receipt-out <PATH>` | — | — | unset | Write bounded `<= 2,048 B` `CompactReceipt` JSON (`pyrn.receipt.v1`) to `PATH`. |
| `--json-out <PATH>` | — | — | unset | Write full pretty-printed `RunReport` JSON to `PATH`. |
| `--json` | — | — | `false` | Print structured `pyrn.v1` JSON envelope (receipt, budget, metrics, impact) to `stdout`. |
| `--ndjson` | — | — | `false` | Stream `StreamEvent` NDJSON lines to `stdout`. |
| `--reruns <N>` | — | — | `0` | Auto-rerun non-`TimedOut` failing tests up to `N` times to classify flakes (`ExecVerdict::Flaky`). |
| `--test-timeout <SECS>` | — | — | `60` | Hard per-test watchdog timeout in seconds (clamped to `>= 1`). Collection watchdog uses `max(45, 2 * test_timeout)` seconds. |
| `--markers <EXPR>` | `-m` | — | unset | Pytest marker expression (`-m`). Included in CAS environment fingerprint when caching. |
| `--keyword <EXPR>` | `-k` | — | unset | Pytest keyword expression (`-k`). Included in CAS environment fingerprint when caching. |
| `--config-file <PATH>` | `-c` | — | auto (`pyproject.toml` if present) | Explicit pytest configuration file passed to workers. |
| `--cache-dir <DIR>` | — | `PYRN_CACHE_DIR` | `<OS temp>/pyrn-cache` | Base directory for CAS DB, daemon state, timing history, and SQLite templates. |
| `--trace` | — | — | `false` | Collect PEP 669 + PEP 578 + `os.environ` dynamic traces inside workers. |
| `--trace-in <PATH>` | — | — | unset | Load pre-existing `DynamicTraceFile` JSON to enrich TIA edges and cache keys. |
| `--trace-out <PATH>` | — | — | unset | Write collected `DynamicTraceFile` JSON to `PATH`. |
| `--trace-granularity <MODE>` | — | — | `file` | Dynamic `sys.monitoring` restart granularity: `file` restarts once per test file (low overhead); `test` restarts per test item. |
| `--trace-symbols` | — | — | `false` | Also capture per-code-object `rel_path::qualname` symbols during `--trace`. |
| `--commit-sha <SHA>` | — | — | `"working-tree"` | Commit SHA metadata recorded when seeding CAS cache rows. |
| `--no-fast-path` | — | — | `false` | Disable hybrid direct fast-path execution and run every test through full `runtestprotocol`. |
| `--no-shard-heavy` | — | — | `false` | Disable deterministic sub-file sharding for heavy-tail files. |
| `--no-sqlite-template` | — | — | `false` | Disable SQLite `:memory:` and file-backed migration template cloning. |
| `--top-slow <N>` | — | — | `10` | Number of slowest tests and files shown in human diagnostics (`0` disables slow diagnostics). |
| `--show-failures <N>` | — | — | `20` | Maximum failure/fault/flake entries printed in human diagnostics. |

!!! note "Tracing granularity and symbol capture"
    In `pyrn_worker.py`, `--trace-granularity file` (the default) resets `sys.monitoring` one-shot events once per test file, while `--trace-granularity test` resets per test item. Pass `--trace-symbols` alongside `--trace` to record `rel_path::qualname` symbol entries.

See [Execution engine](execution-engine.md), [Test Impact Analysis](tia.md), [Content-addressed caching](caching.md), and [Receipts and telemetry](receipts-and-telemetry.md) for detailed semantics.

---

## `pyrn plan`

Evaluate pure in-memory symbol-level Test Impact Analysis and compute BLAKE3 cache keys without spawning Python workers or executing tests.

```sh
pyrn plan [OPTIONS]
```

If neither `--base` nor `--changed` is provided, `pyrn plan` conservatively selects the full test suite (`effective_tier = "full_verified"`).

### Options

| Flag | Alias | Env var | Default | Description |
| --- | --- | --- | --- | --- |
| `--repo <REPO>` | `--project` | — | `.` | Target repository root directory. |
| `--base <BASE>` | — | — | unset | Git base ref to diff the working tree against (read-only symbol-level AST diff). |
| `--changed <CHANGED>` | — | — | empty | Explicit changed repo-relative paths (repeatable or comma-separated). Takes precedence over `--base` and marks each path `ChangedSymbols::All`. |
| `--tier <TIER>` | — | — | `strict` | Confidence tier: `strict`, `transitive`, or `full`. |
| `--role <ROLE>` | — | — | `agent` | Slot role recorded in the plan receipt (`agent` or `verify`). |
| `--trace-in <PATH>` | — | — | unset | Optional `DynamicTraceFile` JSON to merge runtime Python, asset, directory, and env edges. |
| `--json` | — | — | `false` | Emit structured `pyrn.v1` JSON plan to `stdout`. |
| `--metrics-kv` | — | — | `false` | Emit `<PREFIX> key=<i64> ...` line to `stdout` (when `--json` is not set). |
| `--metrics-prefix <PREFIX>` | — | `PYRN_METRICS_PREFIX` | `PYRN_METRIC` | Prefix for `--metrics-kv`. |
| `--receipt-out <PATH>` | — | — | unset | Write bounded `<= 2,048 B` `CompactReceipt` JSON to `PATH`. |
| `--all-files` | — | — | `false` | Print every selected test file in human mode (default prints up to `30`). |

---

## `pyrn daemon`

Manage the persistent warm Python worker pool daemon segregated by `(role, repo, python)`. See [Execution engine](execution-engine.md#warm-daemon-lifecycle-and-invalidation) for pool recycling and module invalidation guarantees.

### `pyrn daemon start`

Boot the affinity-partitioned worker pool, serving RPC requests in the foreground until `pyrn daemon stop` or `Ctrl-C`.

```sh
pyrn daemon start [OPTIONS] [-- <EXTRA_PYTEST_ARGS>...]
```

| Flag | Short / Alias | Env var | Default | Description |
| --- | --- | --- | --- | --- |
| `--repo <REPO>` | `--project` | — | `.` | Target repository root directory. |
| `--python <PYTHON>` | — | `PYRN_PYTHON` | auto-detected | Python interpreter executable. |
| `--workers <WORKERS>` | `-w` | `PYRN_MAX_WORKERS` (cap under `auto`) | `auto` | Total worker count (`auto` or integer `>= 1`). |
| `--role <ROLE>` | — | — | `agent` | Isolation role (`agent` or `verify`). |
| `--cache-dir <DIR>` | — | `PYRN_CACHE_DIR` | `<OS temp>/pyrn-cache` | Base cache directory holding `daemon.json`. |
| `--markers <EXPR>` | `-m` | — | unset | Default pytest marker expression configured at worker boot. |
| `--config-file <PATH>` | `-c` | — | auto (`pyproject.toml` if present) | Explicit pytest config file. |
| `--no-sqlite-template` | — | — | `false` | Disable SQLite template cloning in daemon workers. |
| `-- <EXTRA_PYTEST_ARGS>...` | — | — | empty | Extra pytest arguments forwarded at worker boot. |

### `pyrn daemon status`

Query status of the active warm daemon for `(role, repo, python)`. Exits `0` when running, `1` when stopped or unreachable, and `2` on invalid arguments.

```sh
pyrn daemon status [OPTIONS]
```

| Flag | Alias | Env var | Default | Description |
| --- | --- | --- | --- | --- |
| `--repo <REPO>` | `--project` | — | `.` | Target repository root directory. |
| `--python <PYTHON>` | — | `PYRN_PYTHON` | auto-detected | Python interpreter executable. |
| `--role <ROLE>` | — | — | `agent` | Isolation role (`agent` or `verify`). |
| `--cache-dir <DIR>` | — | `PYRN_CACHE_DIR` | `<OS temp>/pyrn-cache` | Base cache directory. |
| `--json` | — | — | `false` | Print `pyrn.v1` JSON status envelope (`running: true|false`). |

### `pyrn daemon stop`

Send a graceful shutdown RPC to the active daemon, terminate all warm Python workers in parallel, and remove `daemon.json`. Exits `0` on success, `2` if no active daemon is reachable.

```sh
pyrn daemon stop [OPTIONS]
```

Accepts the same flags as [`pyrn daemon status`](#pyrn-daemon-status) (`--repo`, `--python`, `--role`, `--cache-dir`, `--json`).

---

## `pyrn cache`

Inspect or prune the multi-process SQLite WAL content-addressed test cache (`cas_cache.sqlite3`). See [Content-addressed caching](caching.md).

### `pyrn cache status`

Print cache DB path, file size in KiB, entry counts by role and outcome, cumulative hit count, and distinct project count.

```sh
pyrn cache status [OPTIONS]
```

| Flag | Alias | Env var | Default | Description |
| --- | --- | --- | --- | --- |
| `--repo <REPO>` | `--project` | — | `.` | Target repository root directory. |
| `--python <PYTHON>` | — | `PYRN_PYTHON` | auto-detected | Python interpreter executable. |
| `--role <ROLE>` | — | — | `agent` | Isolation role selecting `<cache_dir>/<role>/<project_fp>/cas_cache.sqlite3`. |
| `--cache-dir <DIR>` | — | `PYRN_CACHE_DIR` | `<OS temp>/pyrn-cache` | Base cache directory. |
| `--json` | — | — | `false` | Print `pyrn.v1` JSON cache statistics envelope. |

### `pyrn cache prune`

Delete cache entries older than `--max-age-days` (`0` deletes all matching entries).

```sh
pyrn cache prune [OPTIONS]
```

| Flag | Alias | Env var | Default | Description |
| --- | --- | --- | --- | --- |
| `--repo <REPO>` | `--project` | — | `.` | Target repository root directory. |
| `--python <PYTHON>` | — | `PYRN_PYTHON` | auto-detected | Python interpreter executable. |
| `--role <ROLE>` | — | — | `agent` | Isolation role selecting the cache DB file (and filter target when `--only-role` is set). |
| `--cache-dir <DIR>` | — | `PYRN_CACHE_DIR` | `<OS temp>/pyrn-cache` | Base cache directory. |
| `--max-age-days <DAYS>` | — | — | `30` | Maximum age in days (`0` clears all matching rows). |
| `--only-role` | — | — | `false` | Restrict deletion inside the SQLite table strictly to `--role` instead of all roles. |
| `--json` | — | — | `false` | Print `pyrn.v1` JSON prune summary envelope. |

---

## `pyrn doctor`

Probe host and container worker resource limits, Python/pytest/coverage/`sys.monitoring` availability, effective discovery configuration, cache and SQLite template status, warm daemon liveness, and the slowest historical test files from `timings.json`.

```sh
pyrn doctor [OPTIONS]
```

| Flag | Alias | Env var | Default | Description |
| --- | --- | --- | --- | --- |
| `--repo <REPO>` | `--project` | — | `.` | Target repository root directory. |
| `--python <PYTHON>` | — | `PYRN_PYTHON` | auto-detected | Python interpreter executable to probe. |
| `--role <ROLE>` | — | — | `agent` | Cache and daemon isolation role (`agent` or `verify`). |
| `--cache-dir <DIR>` | — | `PYRN_CACHE_DIR` | `<OS temp>/pyrn-cache` | Base cache directory. |
| `--top-slow <N>` | — | — | `10` | Number of slowest historical test files to list from `timings.json`. |
| `--json` | — | — | `false` | Emit structured `pyrn.v1` JSON diagnostics (`DoctorReport` + `slowest_historical_files_ms`). |

See [Resource budgeting](resource-budgeting.md#inspecting-budgets-with-pyrn-doctor) for how to interpret the worker budget line.

---

## `pyrn replay-git`

Replay the most recent non-merge commits in a Git repository in strictly read-only mode (`git log`, `git diff-tree`, `git show`) against the current dependency graph to measure co-change recall, node reduction, symbol-pruned edges, and optional duration-weighted speedup.

```sh
pyrn replay-git [OPTIONS]
```

| Flag | Alias | Default | Description |
| --- | --- | --- | --- |
| `--repo <REPO>` | `--project` | `.` | Target Git repository root directory (never modified). |
| `--limit <N>` | — | `30` | Number of recent non-merge commits to replay. |
| `--tier <TIER>` | — | `strict` | Confidence tier evaluated on each commit (`strict`, `transitive`, or `full`). |
| `--trace-in <PATH>` | — | unset | Optional `DynamicTraceFile` JSON enriching static TIA edges and duration hints. |
| `--durations-csv <PATH>` | — | unset | Optional CSV of per-file timings for duration-weighted speedup estimates. |
| `--json` | — | `false` | Emit full `ReplaySummary` JSON to `stdout`. |

`--durations-csv` accepts a header row matching any of `file`, `file_path`, `test_file`, or `path` for the path column, and any of `total_ms`, `duration_ms`, `wall_ms`, `total_s`, `duration_s`, or `seconds` for the duration column (columns ending in `_s` or named `seconds` are multiplied by `1000.0`).

---

## Related guides

- [Getting started](getting-started.md) — installation and first run.
- [Configuration](configuration.md) — `.pyrn.toml`, `pyproject.toml`, and environment variables.
- [Recipes](recipes.md) — copy-paste workflows for local development, Django, and CI.
- [Troubleshooting](troubleshooting.md) — diagnosing failures, exit codes, and cache misses.
