# Changelog

All notable changes to `pyrn` are recorded here and summarized in the root
[CHANGELOG.md](https://github.com/dariuszpanas/pyrn/blob/main/CHANGELOG.md). This project follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-26

### Added

- **First-party GitHub Action (`action.yml`) and ecosystem integration:**
  - Added a reusable composite GitHub Action (`dariuszpanas/pyrn`) supporting `run`, `plan`,
    `doctor`, and `config` with SHA-256 verified standalone binary downloads cached in
    `$RUNNER_TEMP`, safe bash array argument assembly, structured `$GITHUB_OUTPUT` metrics from
    `CompactReceipt`, and `$GITHUB_STEP_SUMMARY` Markdown tables.
  - Added `.pre-commit-hooks.yaml` (`pyrn-doctor`, `pyrn-tia`), copy-paste examples under
    `examples/` (`ci-pytest-coverage.yml`, `tia-shadow.yml`, `pyrn.toml`), and a dedicated
    [GitHub Actions](github-actions.md) guide.
- **Rich CLI presentation and lifecycle subcommands (`crates/pyrn-cli`):**
  - Integrated `fast-rich` (`=0.4.0`) for rounded Unicode panels, aligned key-value tables, styled
    verdict indicators, framed Rich `--help` and stderr error panels, and a global `--plain` flag
    respecting `NO_COLOR`, `FORCE_COLOR`, `GITHUB_ACTIONS`, and TTY width while keeping `--json`,
    `--ndjson`, and `--metrics-kv` completely unframed.
  - Added `pyrn config show|init`, `pyrn self status|update` (detecting `standalone`, `uv-tool`, and
    `cargo` installs), and `pyrn completion bash|zsh|fish|powershell|elvish`.

### Changed

- **Continuous cold `--cov-sysmon` coverage and large-suite batching (`crates/pyrn-runner`):**
  - Keep per-worker `coverage.Coverage` active continuously across work units in cold runs and
    commit once at graceful worker shutdown (extending shutdown drain timeout to `15 s`), removing
    hundreds of per-unit `stop()`/`save()`/`start()` cycles.
  - Avoid over-sharding medium files when large suites already have abundant file-level parallelism,
    and pack consecutive tiny files (up to 6 files / `1,200 ms`) per unit to cut per-unit collection
    overhead and raise worker efficiency.

## [0.1.1] - 2026-11-26

### Fixed

- **Import-time `--cov-sysmon` coverage parity (`crates/pyrn-runner`):**
  - Start the `coverage.Coverage` controller at worker boot before importing `pytest`, framework
    setup hooks, `conftest.py`, or running test collection, and keep it active through
    module/class/function fixture teardown so top-level `import`, `class`, `def`, and module
    constant statements are recorded with `pytest-cov` parity under CPython 3.12+ `sys.monitoring`.
  - Scope per-unit coverage activation cleanly so warm daemon workers do not accidentally reuse a
    previous request's coverage controller when a subsequent run disables `--cov-sysmon`.
- **Relative `--cov-out` path resolution (`crates/pyrn-runner`):**
  - Resolve relative `--cov-out` paths (such as bare `--cov-out .coverage`) against `project_root`
    and skip empty parent directory creation before calling `coverage combine`.

## [0.1.0] - 2026-11-26

First public beta release of `pyrn`.

### Added

- **Pure-Rust symbol-level Test Impact Analysis (`crates/pyrn-core`):**
  - Parallel Python AST parser (`rustpython-parser` + `rayon`) with `def`/`class` symbol-level
    diffs and module-preamble fail-closed guards.
  - Fixture-scoped `conftest.py` resolution (`smart_fixture_scoped` and `strict_all_imports`),
    Django settings/migration/template rules, zero-static inherited `unittest.TestCase` handling,
    confidence tiers (`strict`, `transitive`, `full`), and fail-closed `UnknownPathPolicy::Full`.
  - Pure in-memory default build (`default-features = false`, `#![forbid(unsafe_code)]`) exposing
    `TreeSource`, `InMemoryTree`, and `plan_python_tests` with authoritative `base_tree` policy
    loading and zero FS, SQLite/C FFI, or subprocess dependencies.
  - Optional `fs` (parallel filesystem tree loader), `cache` (SQLite WAL `CasTestCache`), and
    `process` (read-only Git symbol diffing and `replay-git`) Cargo features.
- **Role-segregated BLAKE3 content-addressed test cache (`CasTestCache`):**
  - Multi-process ACID SQLite WAL cache keyed by 64-hex domain-separated `pyrn.test-key.v1`
    closure digests incorporating static/dynamic `os.environ` reads and PEP 669 + PEP 578 traces.
  - Strict `SlotRole::{Agent, Verify}` directory and primary-key isolation; failed, errored,
    skipped, flaky, and tainted tests are never served as cache hits.
- **Isolated cold & warm-pool execution engine (`crates/pyrn-runner`):**
  - Persistent live `pytest.Session` workers with affinity lanes (`hermetic`, `sqlite_django`,
    `serial_external`), LPT + heavy-file sharding, and sticky work-stealing.
  - Container- and SMT-aware `-w auto` worker budgeting (`ResourceBudget`) computing the minimum of
    physical CPU cores, `available_parallelism`, Linux cgroups v2/v1 CPU quota and cpuset,
    container memory OOM guard (`75% / worker_mem_mb`), `PYRN_MAX_WORKERS`, config `max_workers`,
    and scheduled units.
  - BLAKE3-keyed `:memory:` (`sqlite3.backup()`) and file-backed SQLite migration template cloning.
  - Compact Windows `MAX_PATH`-safe RAII sandboxes, stripped ambient pytest/xdist/coverage env
    vars, disjoint loopback port ranges, authenticated loopback TCP IPC (128-bit `getrandom`
    tokens), hard per-test watchdogs (`--test-timeout`), crash respawn with sibling requeue,
    `--reruns` flake detection, opt-in `pure_autouse_guards` hybrid direct fast-path, and built-in
    `--cov-sysmon` shard merging.
  - Strict negative CAS skip-list execution guaranteeing inherited `unittest.TestCase` and
    dynamically generated tests are never silently dropped.
- **Standalone CLI (`crates/pyrn-cli`) and zero-dependency Python wheel (`src/pyrn/`):**
  - Unified `pyrn` subcommands: `run`, `plan`, `daemon start|status|stop`, `cache status|prune`,
    `doctor`, and `replay-git` with stable `0`/`1`/`2` exit codes.
  - Bounded `<= 2,048 B` `CompactReceipt` (`pyrn.receipt.v1`), `<= 128 B` `CrashCapsule`,
    `--ndjson` `StreamEvent` output, and configurable `--metrics-kv` / `--metrics-prefix`
    (`PYRN_METRIC` default, `PYRN_METRICS_PREFIX` env var).
  - Zero-dependency `py3-none-<platform>` Python wheel bundling the standalone Rust `pyrn`
    executable without PyO3.
- **Documentation site:**
  - Complete Zensical documentation with light/dark/system Rust-amber theme, animated `PyRn`
    wordmark, stdlib-only link and fragment checker (`scripts/check_docs.py`), CI docs job, and
    GitHub Pages deployment workflow.

## How entries are organized

Keep new user-visible changes under `## [Unreleased]` using the standard Keep a Changelog headings:

| Heading | Use it for |
| --- | --- |
| `Added` | New commands, configuration keys, library APIs, or capabilities |
| `Changed` | Changes to existing behavior, defaults, or output contracts |
| `Deprecated` | Supported behavior scheduled for removal |
| `Removed` | Behavior or interfaces no longer present |
| `Fixed` | Bug fixes and soundness or parity regressions |
| `Security` | Vulnerability fixes or trust-boundary changes |
