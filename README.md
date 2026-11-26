# `pyrn`

**Rust-powered Python/pytest Test Impact Analysis (TIA), content-addressed test caching, and isolated warm-pool execution engine** — built in pure safe Rust (`edition = "2024"`, `#![forbid(unsafe_code)]`) and packaged as both a standalone Rust CLI and a zero-dependency Python platform wheel.

## Why `pyrn`?

Startup-only pytest wrappers barely move the needle on large Python suites, while `pytest-xdist` wastes tens of seconds on redundant worker collection, oversubscribes SMT/container CPU and memory limits, leaks shared temp paths and ports, and aborts entire runs when a worker crashes.

`pyrn` solves **every** stage of the pytest lifecycle in one cohesive Rust workspace:

1. **Zero-Checkout Symbol-Level Python TIA (`crates/pyrn-core`)**:
   - Pure-Rust Python AST parsing (`rustpython-parser` + `rayon`) with symbol-level diffs (`def`/`class` granularity), fixture-scoped `conftest.py` resolution, Django settings/migration/template rules, and fail-closed `FullVerified` fallback (`UnknownPathPolicy::Full`).
   - Compiles cleanly with **`--no-default-features`** (zero subprocesses, zero SQLite/C FFI, zero filesystem walking) so host tools and CI services can embed `pyrn-core` directly over in-memory or virtual repository trees via `TreeSource` and `plan_python_tests`.
2. **Role-Segregated BLAKE3 Content-Addressed Test Cache (`CasTestCache`)**:
   - Multi-process ACID SQLite WAL cache keyed by 64-hex domain-separated BLAKE3 closure digests (`pyrn.test-key.v1`), incorporating static and dynamic `os.environ` reads (`DJANGO_*`/`RAY_*` allowlist + runtime proxy) and PEP 669 `sys.monitoring` + PEP 578 audit traces.
   - Strictly segregated by `SlotRole::{Agent, Verify}`: failed, skipped, errored, or tainted tests are never cached, and interactive `agent` entries never bleed into clean `verify` runs.
3. **Isolated Warm Execution Engine & SQLite Template Cloning (`crates/pyrn-runner`)**:
   - Affinity-partitioned persistent worker pool (`--daemon`) and cold pool with short isolated sandboxes (preventing Windows `MAX_PATH` failures), `:memory:` `sqlite3.backup()` + file-backed SQLite migration template cloning, automatic crash/timeout worker respawn + remaining-item requeue, `--reruns` flake detection (`ExecVerdict::Flaky`), and built-in `--cov-sysmon` (`COVERAGE_CORE=sysmon`).
   - **Container- & SMT-aware `-w auto` (`resources.rs`)**: computes `min(physical_cpus, affinity, cgroup v2/v1 CPU quota, cpuset, memory OOM guard)` so SMT workstations and Docker/Kubernetes/GitHub Actions runners never thrash or OOMKill (`exit 137`).
4. **Zero-Dependency Python Platform Wheel (`src/pyrn/` + `scripts/native_wheel.py`)**:
   - Ships a `py3-none-<platform>` wheel with zero PyO3 and zero Python runtime dependencies, launching the bundled `pyrn(.exe)` binary directly via `os.execve` (POSIX) or `subprocess.call` (Windows).

## Benchmark Highlights

Measured on a 12,000-test Django + Ray codebase:

- **Pure-Rust symbol-level TIA**: parses and graphs the entire repository in **~130 ms** with **100% co-change recall** across real Git history, cutting typical PR/commit test suites by **70–95%** while breaking hub-module fanout via `def`/`class`-level symbol pruning.
- **Content-addressed test cache (`--cache`)**: serves unchanged passing test closures in milliseconds with **zero worker spawns** on 100% cache hits, while strictly re-running tainted, failed, or env-invalidated tests.
- **Isolated warm pool + SQLite template cloning (`--daemon`)**: eliminates repeated pytest collection and Django migration overhead across iterations, while recovering cleanly from hard worker crashes (`os._exit(139)`) and per-test timeouts without losing sibling test results.
- **Built-in `--cov-sysmon`**: leverages Python 3.12+ `COVERAGE_CORE=sysmon` with isolated per-worker coverage shards merged cleanly outside the repo tree (typically **2.4×–3.1× faster** than C-trace coverage).

## Workspace Layout

| Path | Kind | Responsibility |
| :--- | :--- | :--- |
| `crates/pyrn-core` | `lib` | Pure-Rust in-memory TIA, symbol-level AST diff, BLAKE3 cache keys, `.pyrn.toml` / `pyproject.toml` config, `CompactReceipt` (`<= 2 KiB`), `CrashCapsule` (`<= 128 B`), `RunMetrics`; optional `fs`, `cache`, `process` features |
| `crates/pyrn-runner` | `lib` | Isolated cold/warm worker supervisor, container-aware `ResourceBudget`, unified TIA+CAS run pipeline, SQLite template cloning, PEP 669 + PEP 578 + `os.environ` tracer, `--cov-sysmon`, `pyrn doctor` |
| `crates/pyrn-cli` | `bin: pyrn` | Standalone CLI (`run`, `plan`, `daemon`, `cache`, `doctor`, `replay-git`) with deterministic exit codes and configurable `--metrics-kv` telemetry |
| `src/pyrn/` + `scripts/native_wheel.py` | Python wheel | Zero-dependency `py3-none-<platform>` launcher wheel (`hatch_build.py`) |

## Quickstart

### Build & Test (Rust)

```sh
# 1. Verify pure-Rust zero-FFI in-memory library build
cargo check -p pyrn-core --no-default-features

# 2. Lint and run full workspace test suite
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --release

# 3. Build release CLI binary
cargo build --release --workspace
```

### Build Zero-Dependency Python Wheel

```sh
# Reuses an existing release binary (fast) or compiles pyrn-cli from source automatically:
PYRN_PREBUILT_BINARY=target/release/pyrn uv build --wheel
```

On Windows PowerShell:

```powershell
$env:PYRN_PREBUILT_BINARY = "target\release\pyrn.exe"
uv build --wheel
```

### Everyday CLI Usage

```sh
# Inspect host/container worker budget, Python/pytest/sys.monitoring, cache & slow history
pyrn doctor --repo .

# Pure in-memory Test Impact Analysis plan (no test execution)
pyrn plan --repo . --base HEAD --tier strict --json

# Fast full run with automatic physical-core + cgroup sizing
pyrn run --repo . -w auto

# Unified TIA + CAS cache + warm daemon run with structured receipt & KV metrics
pyrn daemon start --repo . -w auto --role agent
pyrn run --repo . --tia --base HEAD --cache --daemon --role agent --metrics-kv --receipt-out receipt.json
pyrn daemon status --repo .
pyrn daemon stop --repo .

# Customize CI key-value metrics prefix (or set PYRN_METRICS_PREFIX env var)
pyrn run --repo . --tia --base HEAD --role verify --metrics-kv --metrics-prefix CI_METRIC

# Built-in Python 3.12+ sys.monitoring coverage
pyrn run --repo . --cov-sysmon --cov-out .coverage

# Inspect or prune SQLite WAL CAS test cache
pyrn cache status --repo .
pyrn cache prune --repo . --max-age-days 30

# Read-only Git commit history TIA replay
pyrn replay-git --repo . --limit 30 --tier strict
```

## Configuration (`.pyrn.toml` / `pyproject.toml`)

`pyrn` automatically reads `[tool.pytest.ini_options]` and `[tool.pyrn]` from `pyproject.toml`, with optional overrides in `.pyrn.toml`:

```toml
testpaths = ["tests"]
python_files = ["test_*.py", "*_test.py"]

[tia]
symbol_level_tia = true
conftest_policy = "smart_fixture_scoped"   # or "strict_all_imports"
unknown_path_policy = "full"               # fail closed on unmapped files
env_allowlist_prefixes = ["DJANGO_", "RAY_"]
extra_full_triggers = []

[runner]
worker_mem_mb = 300
pure_autouse_guards = []
```

## Embedding `pyrn-core` (Zero-Checkout Library API)

Downstream Rust tools can depend on `pyrn-core` with `default-features = false` (zero C/SQLite FFI, zero subprocesses, zero disk checkout) and evaluate `plan_python_tests(base_tree, candidate_tree, dynamic_traces, &PlanOptions::default())` directly over any `TreeSource` implementation.

## Exit Codes

| Code | Meaning |
| :--- | :--- |
| `0` | All selected tests passed (or passed on rerun as `flaky`), or informational command succeeded |
| `1` | Deterministic test failure, timeout, crash (`failed` / `timed_out`), or stopped daemon status |
| `2` | Invalid CLI usage, configuration error, or operational/runtime error |
