# Get started

`pyrn` ships from a single Rust workspace as both a standalone `pyrn` CLI executable and a
zero-dependency `py3-none-<platform>` Python platform wheel (no PyO3 and no Python runtime
dependencies). Both launch the exact same Rust binary and share identical commands, configuration,
and exit codes.

## Prerequisites

- **Target repository environment:** Python 3.11+ with `pytest` installed (Python 3.12+ recommended
  for PEP 669 `sys.monitoring` dynamic tracing and `--cov-sysmon` coverage, plus the `coverage`
  package when using `--cov-sysmon`).
- **Building from source:** Rust stable toolchain (edition 2024) and `uv` if building the Python
  wheel.

## Install or build `pyrn`

### Install the Rust CLI with Cargo

From a reviewed checkout of the repository:

```sh
cargo install --locked --path crates/pyrn-cli
pyrn --version
```

Or build the release binary in place without installing globally:

```sh
cargo build --release --workspace
./target/release/pyrn --version
```

### Build and install the zero-dependency Python wheel

The wheel hook (`scripts/native_wheel.py`) automatically compiles `pyrn-cli` or reuses an already
built release executable via `PYRN_PREBUILT_BINARY`:

On Linux and macOS:

```sh
cargo build --release --locked -p pyrn-cli
PYRN_PREBUILT_BINARY=target/release/pyrn uv build --wheel
uv tool install dist/pyrn-*.whl
pyrn --version
```

On Windows PowerShell:

```powershell
cargo build --release --locked -p pyrn-cli
$env:PYRN_PREBUILT_BINARY = "target\release\pyrn.exe"
uv build --wheel
uv tool install (Get-ChildItem dist\pyrn-*.whl | Select-Object -First 1).FullName
pyrn --version
```

You can also invoke the wheel as `python -m pyrn --version` inside any virtual environment where
it is installed.

## 1. Inspect your environment with `pyrn doctor`

Run `pyrn doctor` inside your target Python repository before your first test run:

```sh
pyrn doctor --repo .
```

`pyrn doctor` reports:

- **Worker budget:** the worker count `-w auto` will select and the exact limiting factor
  (physical cores, scheduler affinity, Linux cgroup v2/v1 CPU quota, cpuset, memory OOM guard,
  `PYRN_MAX_WORKERS`, or `.pyrn.toml` `max_workers`).
- **Python environment:** auto-detected interpreter (`.venv` / `venv` / `VIRTUAL_ENV` or explicit
  `--python` / `PYRN_PYTHON`), Python version, `pytest` version, `coverage` version, and whether
  `sys.monitoring` is available.
- **Discovery and cache layout:** effective `testpaths`, `python_files`, opt-in
  `pure_autouse_guards`, cache directory, SQLite CAS database path, SQLite template status, and
  warm daemon state.
- **Historical slow files:** top slowest files once timing history exists (controlled with
  `--top-slow`).

Pass `--json` for structured machine output.

## 2. Preview impacted tests with `pyrn plan`

`pyrn plan` performs pure in-memory symbol-level Test Impact Analysis without spawning Python or
running tests:

```sh
# Diff working tree against HEAD at default strict tier
pyrn plan --repo . --base HEAD --tier strict

# Or test explicit repository-relative changed paths
pyrn plan --repo . --changed src/app/service.py --tier strict --json
```

`pyrn plan` prints selected test files, test node reduction percentage, symbol-pruned edges,
AST parse/graph timings, and any fail-closed `FullVerified` fallback reasons (such as edits to
`pyproject.toml`, lockfiles, or unmapped non-Python files).

## 3. Run tests in parallel with `-w auto`

Execute your suite using isolated cold workers sized automatically to your machine or container:

```sh
pyrn run --repo . -w auto
```

Target specific paths, pytest markers, or keyword filters just like `pytest`:

```sh
pyrn run --repo . -w auto -m "not slow" -k "auth" tests/unit/
```

Extra pytest flags can be forwarded after `--`:

```sh
pyrn run --repo . -w auto tests/unit/ -- --maxfail=3
```

Every `pyrn run` prints outcomes, wall/setup/DB-template timings, and a built-in **"Why Was This
Run Slow?"** breakdown (setup/call/teardown split, worker efficiency, tail skew, and top slowest
tests and files).

### Stable exit codes

| Exit code | Meaning |
| --- | --- |
| `0` | All selected tests passed (or passed on rerun as `flaky`), or informational command succeeded |
| `1` | Deterministic test failure, timeout, crash (`failed` / `timed_out`), or stopped daemon status |
| `2` | Invalid CLI usage, configuration error, or operational/runtime error |

## 4. Enable the persistent warm-daemon TDD loop

For iterative local development, start a persistent warm worker pool so Python startup,
`conftest.py` loading, `django.setup()`, and SQLite templates stay resident across runs:

```sh
# Terminal 1 (foreground daemon; stops on Ctrl-C or `pyrn daemon stop`)
pyrn daemon start --repo . -w auto --role agent
```

In another terminal, combine symbol-level TIA, the BLAKE3 content-addressed cache, and the warm
pool:

```sh
# Run only impacted, uncached tests on warm workers
pyrn run --repo . --tia --base HEAD --cache --daemon --role agent

# Check daemon health and runs served
pyrn daemon status --repo .

# Inspect or prune the SQLite WAL cache
pyrn cache status --repo .
pyrn cache prune --repo . --max-age-days 30

# Stop the daemon cleanly when finished
pyrn daemon stop --repo .
```

The warm daemon automatically detects edits to source files, `conftest.py`, `pyproject.toml`,
`.pyrn.toml`, or migrations via content-hashed BLAKE3 epochs and recycles workers transparently.
When only test files change (including same-size edits or deletions), it broadcasts `Invalidate`
to every worker without restarting Python.

## Next steps

- Rolling `pyrn` into a team repository? Read [Assess and adopt in CI](ci-adoption.md).
- Looking for copy-paste workflows? Browse [Recipes](recipes.md).
- Need full CLI flags or config keys? See [Commands](commands.md) and
  [Configuration](configuration.md).
