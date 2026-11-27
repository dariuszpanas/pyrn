# Changelog

All notable changes to `pyrn` are maintained in [docs/changelog.md](docs/changelog.md) and
published at [dariuszpanas.github.io/pyrn/changelog.html](https://dariuszpanas.github.io/pyrn/changelog.html).

This project follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.3] - 2026-11-27

### Fixed

- **Enforce `testpaths` in `DependencyGraph` (`crates/pyrn-core`)**:
  - Exclude `test_*.py` helper scripts outside configured `testpaths` (e.g. `scripts/test_*.py`)
    from `tests_by_file` so `--tia` and `--cache` match pytest/discovery test selection and never
    attempt to execute CLI helper scripts as pytest test modules.

## [0.2.2] - 2026-11-27

### Fixed

- **Fail-closed TIA on git diff errors (`crates/pyrn-runner`)**:
  - Fall back to `FullVerified` (running the entire suite) instead of selecting zero tests when
    `git merge-base` or `git diff` against `--base` fails (e.g. shallow CI checkouts).
- **Exact `merge-base` diffing for branches (`crates/pyrn-core`)**:
  - Resolve `git merge-base <base> HEAD` whenever `--base` is not `HEAD` so PR merge commits and
    branches behind `origin/main` diff only their own changes.
- **Bypass CAS reads on TIA `FullVerified` fallback (`crates/pyrn-runner`)**:
  - Skip serving cached passing hits whenever TIA fails closed to `FullVerified` (e.g. unmapped
    non-Python file or global trigger change), executing all selected tests cold while still
    seeding fresh cache entries afterwards.

## [0.2.1] - 2026-11-26

### Fixed

- **Prevent `FORCE_COLOR`/`CLICOLOR_FORCE` bleed into test workers (`action.yml`, `crates/pyrn-runner`)**:
  - Stop exporting `FORCE_COLOR=1` in `action.yml` (Rich presentation already detects
    `GITHUB_ACTIONS`) and strip ambient `FORCE_COLOR`/`CLICOLOR_FORCE` from worker environments
    so subprocess pytest assertions inside user test suites never receive unexpected ANSI escape
    codes.
- **Actionable failure diagnostics (`crates/pyrn-cli`)**:
  - Display the trailing 14 lines (including highlighted `E ` assertion lines) in the Rich
    `Failures / Faults / Flakes` panel instead of truncating after the first 6 lines of traceback
    header.

## [0.2.0] - 2026-11-26

### Added

- **First-party GitHub Action (`action.yml`) & ecosystem templates**:
  - Reusable composite action (`dariuszpanas/pyrn`) supporting `run`, `plan`, `doctor`, and
    `config`, SHA-256 verified standalone binary bootstrap cached in `$RUNNER_TEMP`, `$GITHUB_OUTPUT`
    receipt metrics, and rich `$GITHUB_STEP_SUMMARY` Markdown tables.
  - Added `.pre-commit-hooks.yaml` (`pyrn-doctor`, `pyrn-tia`), ready-to-copy workflow/config
    templates under `examples/`, and [docs/github-actions.md](docs/github-actions.md).
- **Rich terminal UI & new CLI subcommands (`crates/pyrn-cli`)**:
  - Powered by `fast-rich` (`=0.4.0`): rounded Unicode panels, styled tables, verdict badges,
    framed Rich `--help` / error output, and global `--plain` flag honoring `NO_COLOR`,
    `FORCE_COLOR`, `GITHUB_ACTIONS`, and TTY state (while keeping `--json`, `--ndjson`, and
    `--metrics-kv` pure).
  - Added `pyrn config show|init`, `pyrn self status|update`, and
    `pyrn completion bash|zsh|fish|powershell|elvish`.

### Changed

- **Continuous cold `--cov-sysmon` coverage & large-suite scheduler (`crates/pyrn-runner`)**:
  - Keep the per-worker `coverage.Coverage` controller running continuously across units during cold
    runs and save once on graceful worker shutdown, eliminating hundreds of per-unit SQLite flush
    cycles and increasing statement coverage parity.
  - Tail-aware heavy-file sharding and tiny-file batching (`<= 6` files / `1,200 ms` budget) cut
    per-file collection overhead and improve worker parallel efficiency on large multi-hundred-file
    suites.

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
