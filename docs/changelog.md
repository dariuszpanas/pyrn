# Changelog

All notable changes to `pyrn` are recorded here and summarized in the root
[CHANGELOG.md](https://github.com/dariuszpanas/pyrn/blob/main/CHANGELOG.md). This project follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
