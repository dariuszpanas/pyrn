# Changelog

All notable changes to `pyrn` are maintained in [docs/changelog.md](docs/changelog.md) and
published at [dariuszpanas.github.io/pyrn/changelog.html](https://dariuszpanas.github.io/pyrn/changelog.html).

This project follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-11-26

### Fixed

- **Import-time `--cov-sysmon` coverage parity (`crates/pyrn-runner`)**: start worker coverage
  controllers at worker boot before importing `pytest`, framework hooks (`django.setup()`),
  `conftest.py`, or collecting test modules, and keep coverage active through module/class/function
  fixture teardown so top-level imports, `class`/`def` definitions, constants, and teardown paths
  match `pytest-cov` totals under CPython 3.12+ `sys.monitoring`.
- **Relative `--cov-out` path handling (`crates/pyrn-runner`)**: resolve relative `--cov-out`
  paths (including bare filenames such as `--cov-out .coverage`) against the project root before
  creating parent directories and invoking `coverage combine`.

## [0.1.0] - 2026-11-26

First public beta release of `pyrn`.

### Added

- **Pure-Rust symbol-level Test Impact Analysis (`crates/pyrn-core`)**: parallel AST parser
  (`rustpython-parser` + `rayon`), `def`/`class` symbol-level diffs with preamble fail-closed
  guards, fixture-scoped `conftest.py` resolution, Django settings/migration/template rules,
  fail-closed `UnknownPathPolicy::Full`, and zero-checkout in-memory embedding via `TreeSource`
  and `plan_python_tests` (`default-features = false`, `#![forbid(unsafe_code)]`).
- **Role-segregated BLAKE3 content-addressed test cache (`CasTestCache`)**: multi-process SQLite
  WAL cache keyed by domain-separated 64-hex `pyrn.test-key.v1` digests, strictly segregated by
  `SlotRole::{Agent, Verify}` and refusing to cache failed, skipped, errored, flaky, or tainted
  tests.
- **Isolated cold & warm-pool execution engine (`crates/pyrn-runner`)**: persistent `pytest.Session`
  workers, container- and SMT-aware `-w auto` worker budgeting (physical cores,
  `available_parallelism`, Linux cgroups v2/v1 CPU/cpuset/memory OOM guard), `:memory:` and
  file-backed SQLite migration template cloning, compact Windows-safe sandboxes, disjoint loopback
  ports, built-in PEP 669 + PEP 578 + `os.environ` tracing, `--cov-sysmon` coverage shard merging,
  hard watchdogs, crash respawn, and `--reruns` flake detection.
- **Standalone CLI (`crates/pyrn-cli`) & zero-dependency Python wheel (`src/pyrn/`)**: unified
  `pyrn` binary (`run`, `plan`, `daemon`, `cache`, `doctor`, `replay-git`), bounded `<= 2,048 B`
  `CompactReceipt` (`pyrn.receipt.v1`), `<= 128 B` `CrashCapsule`, `--ndjson` streaming,
  configurable `--metrics-kv` / `--metrics-prefix`, deterministic `0`/`1`/`2` exit codes, and a
  `py3-none-<platform>` wheel with zero PyO3 or Python runtime dependencies.
- **Documentation site**: full Zensical documentation published to GitHub Pages with offline link
  and anchor verification (`scripts/check_docs.py`).
