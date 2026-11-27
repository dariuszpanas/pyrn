# Configuration

`pyrn` reads test discovery settings directly from your existing `pyproject.toml` (`[tool.pytest.ini_options]`, `[tool.hatch.build.targets.wheel]`, and `[tool.pyrn]`) and supports optional repository overrides in `.pyrn.toml` or `pyrn.toml`. Configuration loading operates purely over repository bytes (`PyrnConfig::from_tree`), so local CLI runs and embedded zero-checkout planners evaluate identical rules.

## Precedence order

Settings merge from lowest to highest precedence:

1. **Built-in defaults** (`PyrnConfig::default()`).
2. **`pyproject.toml` — `[tool.pytest.ini_options]`**: loads `testpaths`, `python_files`, `python_classes`, and `python_functions` (accepting either a TOML array of strings or a whitespace-delimited string).
3. **`pyproject.toml` — `[tool.hatch.build.targets.wheel].packages`**: extracts parent source roots before the final `/` (for example, `"src/myapp"` adds `"src"` to `source_roots`).
4. **`pyproject.toml` — `[tool.pyrn]`**: applies top-level keys, then nested `[tool.pyrn.runner]` and `[tool.pyrn.tia]` sections.
5. **`.pyrn.toml`**: overrides values from `pyproject.toml`; nested `[runner]` and `[tia]` override top-level keys within the same file.
6. **`pyrn.toml`**: overrides `.pyrn.toml` if both files exist in the repository root.
7. **Environment variables and CLI flags**: runtime flags (`--workers`, `--tier`, `--role`, `--python`, `--cache-dir`, `--metrics-prefix`) and environment variables (`PYRN_MAX_WORKERS`, `PYRN_WORKER_MEM_MB`, `PYRN_PYTHON`, `PYRN_CACHE_DIR`, `PYRN_METRICS_PREFIX`) apply at invocation time.

!!! note "Policy authority in embedded zero-checkout planning"
    When `pyrn-core`'s [`plan_python_tests`](embedding.md#authoritative-base-vs-candidate-trees) receives a non-empty `base_tree`, it loads `PyrnConfig` from `base_tree` rather than `candidate_tree`. A candidate pull request cannot weaken `unknown_path_policy`, `conftest_policy`, or `extra_full_triggers` to suppress its own test selection.

!!! warning "Graceful parse fallback"
    `PyrnConfig::from_tree` and `PyrnConfig::load_from_dir` ignore syntactically invalid TOML blocks and retain prior or default values instead of aborting. Run `pyrn doctor --repo .` to confirm the effective `testpaths`, `python_files`, `pure_autouse_guards`, and worker budget actually loaded.

## Configuration file reference

Every field of `PyrnConfig` is listed below with its TOML placement, type, and built-in default.

| Field | TOML section(s) | Type | Default | Semantics |
| --- | --- | --- | --- | --- |
| `testpaths` | `[tool.pytest.ini_options]`, top-level | `list[str]` (or whitespace string in `ini_options`) | `["tests"]` | Root directories walked for test discovery and zero-static test registration. |
| `python_files` | `[tool.pytest.ini_options]`, top-level | `list[str]` (or whitespace string in `ini_options`) | `["test_*.py", "*_test.py"]` | Wildcard (`*`, `?`) patterns matched against test basenames. |
| `python_classes` | `[tool.pytest.ini_options]`, top-level | `list[str]` (or whitespace string in `ini_options`) | `["Test*"]` | Wildcard (`*`, `?`) patterns matched against Python test class names. |
| `python_functions` | `[tool.pytest.ini_options]`, top-level | `list[str]` (or whitespace string in `ini_options`) | `["test_*"]` | Wildcard (`*`, `?`) patterns matched against test function and method names. |
| `source_roots` | `[tool.hatch.build.targets.wheel].packages` (auto-extracted) | `list[str]` | `[]` (+ auto `"src"` if `src/*/__init__.py` exists, plus repo root) | Source directories stripped when mapping file paths to dotted Python module names. Not parsed from `.pyrn.toml`; detected from `pyproject.toml` or repository layout (or set via `PyrnConfig` in Rust). |
| `pure_autouse_guards` | top-level, `[runner]` | `list[str]` | `[]` | Opt-in names of project `conftest.py` autouse fixtures verified to be pure/DB-conditional guards. Enables both TIA fixture-scoped import filtering and hybrid direct fast-path elision (never elided when explicitly requested as a test parameter). |
| `max_workers` | top-level, `[runner]` | `usize` (positive; `0` ignored) | unset | Hard upper cap on `-w auto` worker selection (`limiting_factor = "config_max_workers"`). |
| `worker_mem_mb` | top-level, `[runner]` | `u64` MiB (positive; `0` ignored) | unset (effective default `300`) | Estimated resident memory per worker used by the container cgroup OOM guard (`floor(mem_mb * 0.75 / worker_mem_mb)`). Overridden by `PYRN_WORKER_MEM_MB`. |
| `env_allowlist_prefixes` | top-level, `[tia]` | `list[str]` | `["DJANGO_", "RAY_"]` | Environment variable name prefixes always hashed into `pyrn.test-key.v1` cache keys whenever present in the environment. |
| `extra_full_triggers` | top-level, `[tia]` | `list[str]` | `[]` | Additional repo-relative paths whose modification forces `ConfidenceTier::FullVerified` (appended to the built-in global trigger list). |
| `conftest_policy` | top-level, `[tia]` | `"smart_fixture_scoped"` \| `"strict_all_imports"` | `"smart_fixture_scoped"` | Controls how `conftest.py` local fixture imports fan out under `strict` tier. `transitive` and `full` tiers always force `"strict_all_imports"`. |
| `unknown_path_policy` | top-level, `[tia]` | `"full"` \| `"mapped_only"` | `"full"` | Fail-closed policy when a changed file matches neither a known Python module/asset edge nor a known non-impacting file (`.md`, `.txt`, etc.). `"full"` upgrades the run to `FullVerified`; `"mapped_only"` runs only tests with explicit static or dynamic edges. |
| `symbol_level_tia` | top-level, `[tia]` | `bool` | `true` | Enables `def`/`class`/constant symbol-level AST diffing, reverse BFS edge pruning, and symbol-scoped closure hashing under `strict` tier. Always disabled under `transitive` and `full` tiers. |

### Standalone `.pyrn.toml` example

```toml
# .pyrn.toml
testpaths = ["tests"]
python_files = ["test_*.py", "*_test.py"]
python_classes = ["Test*"]
python_functions = ["test_*"]

[tia]
symbol_level_tia = true
conftest_policy = "smart_fixture_scoped"
unknown_path_policy = "full"
env_allowlist_prefixes = ["DJANGO_", "RAY_", "APP_FEATURE_"]
extra_full_triggers = ["config/runtime_defaults.yaml"]

[runner]
max_workers = 8
worker_mem_mb = 350
pure_autouse_guards = ["_reset_in_memory_registry"]
```

### Embedded `pyproject.toml` example

```toml
# pyproject.toml
[tool.pytest.ini_options]
testpaths = ["tests"]
python_files = ["test_*.py", "*_test.py"]

[tool.hatch.build.targets.wheel]
packages = ["src/myservice"]

[tool.pyrn.tia]
symbol_level_tia = true
conftest_policy = "smart_fixture_scoped"
unknown_path_policy = "full"
env_allowlist_prefixes = ["DJANGO_", "MYSERVICE_"]

[tool.pyrn.runner]
worker_mem_mb = 300
```

## Environment variables

### Runtime CLI and scheduler variables

| Variable | Read by | Default / Fallback | Effect |
| --- | --- | --- | --- |
| `PYRN_PYTHON` | `run`, `daemon *`, `cache *`, `doctor` | Auto-detect `.venv` / `venv` / `VIRTUAL_ENV` / `python(3)` | Default value for `--python`. Overridden by explicit `--python`. |
| `PYRN_CACHE_DIR` | `run`, `daemon *`, `cache *`, `doctor` | `<OS temp>/pyrn-cache` | Default value for `--cache-dir` storing CAS SQLite DB, `daemon.json`, `timings.json`, and SQLite templates. |
| `PYRN_MAX_WORKERS` | `pyrn-runner` (`resources.rs`) | unset | Positive integer cap applied during `-w auto` resolution (`limiting_factor = "env_PYRN_MAX_WORKERS"`). Ignored when explicit `-w N` is passed on the CLI. |
| `PYRN_WORKER_MEM_MB` | `pyrn-runner` (`resources.rs`) | config `worker_mem_mb`, else `300` | Positive integer MiB per worker for container memory OOM budgeting. Overrides `worker_mem_mb` in `.pyrn.toml` / `pyproject.toml`. |
| `PYRN_METRICS_PREFIX` | `run`, `plan` | `PYRN_METRIC` | Default value for `--metrics-prefix`. Empty or whitespace-only values fall back to `PYRN_METRIC`. |
| `DJANGO_SETTINGS_MODULE` | `run`, `plan` | `"default"` in `GraphConfig` | Included in `env_fingerprint` (`pyrn.env-fp.v1`) and allowlisted via `DJANGO_` prefix in `pyrn.test-key.v1` cache keys. |
| `VIRTUAL_ENV` | `pyrn-cli` interpreter resolution | unset | Checked after project `.venv`/`venv` when `--python` / `PYRN_PYTHON` is omitted. |

### Launcher and wheel build variables

| Variable | Read by | Default | Effect |
| --- | --- | --- | --- |
| `PYRN_BIN` | `src/pyrn/entrypoint.py` | Bundled `pyrn/_bin/pyrn(.exe)` | Overrides the path to the native `pyrn` executable invoked by the Python wheel console script (`pyrn` / `python -m pyrn`). |
| `PYRN_PREBUILT_BINARY` | `scripts/native_wheel.py` | unset (compiles via `cargo build --release`) | Packages an already-compiled `pyrn` / `pyrn.exe` binary into the `py3-none-<platform>` wheel without running Cargo. |
| `PYRN_WHEEL_PLATFORM` | `scripts/native_wheel.py` | Derived from `rustc --print host-tuple` and `sysconfig.get_platform()` | Explicit wheel platform tag (for example, `manylinux_2_17_x86_64`, `macosx_11_0_arm64`, `win_amd64`). Required if using `PYRN_PREBUILT_BINARY` without `rustc` on `PATH`. |
| `PYRN_CARGO_TARGET` | `scripts/native_wheel.py` | Host target triple from `rustc` | Cross-compilation `--target` triple passed to `cargo build --release --package pyrn-cli`. |

### Test suite and benchmark variables

| Variable | Read by | Default | Effect |
| --- | --- | --- | --- |
| `PYRN_TEST_PYTHON` | `crates/pyrn-runner/tests/integration_runner.rs` | Active `VIRTUAL_ENV`, then `python` / `python3` with `pytest` | Selects the Python interpreter used by `pyrn-runner`'s Rust E2E integration tests. |
| `PYRN_BENCH_REPO` | `crates/pyrn-core/tests/core_tests.rs` | unset (benchmark test skipped) | Points optional ignored benchmark tests at an external Python repository. |
| `PYRN_BENCH_TIMINGS_CSV` | `crates/pyrn-core/tests/core_tests.rs` | unset | Optional per-file durations CSV for duration-weighted TIA benchmark tests. |

### Worker-injected and sanitized variables

Each worker process spawned by `pyrn-runner` receives a sterile sandbox environment with `PYTEST_XDIST_*`, `PYTEST_ADDOPTS`, `PYTEST_CURRENT_TEST`, and ambient `COVERAGE_*` stripped, and receives `PYRN_WORKER_ID`, `PYRN_PORT_START`, `PYRN_PORT_END`, `DJANGO_LIVE_TEST_SERVER_ADDRESS`, isolated `TMPDIR`/`TEMP`/`TMP`, and (under `--cov-sysmon`) `COVERAGE_CORE=sysmon` + per-worker `COVERAGE_FILE`. See [Execution engine](execution-engine.md#per-worker-sandboxing-and-environment-sanitization) for the complete list.

## Related guides

- [Resource budgeting](resource-budgeting.md) — exact precedence between `-w`, `PYRN_MAX_WORKERS`, `max_workers`, `PYRN_WORKER_MEM_MB`, and Linux cgroups.
- [Test Impact Analysis](tia.md) — `conftest_policy`, `unknown_path_policy`, `symbol_level_tia`, and `extra_full_triggers`.
- [Content-addressed caching](caching.md) — `env_allowlist_prefixes` and environment-dependent cache keys.
- [CI adoption](ci-adoption.md) — recommended settings for PR vs merge-queue pipelines.
