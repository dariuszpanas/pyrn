# Contributing

Keep changes focused, test-backed, and explicit about any CLI flag, configuration key, receipt
schema, or library API boundary they affect. See
[CONTRIBUTING.md](https://github.com/dariuszpanas/pyrn/blob/main/CONTRIBUTING.md) in the repository
root for the quick-reference checklist.

## Workspace layout

| Path | Kind | Responsibility |
| --- | --- | --- |
| `crates/pyrn-core` | Rust library | Pure-Rust in-memory TIA, symbol-level AST diff, BLAKE3 cache keys, `.pyrn.toml` / `pyproject.toml` config, `CompactReceipt` (`<= 2 KiB`), `CrashCapsule` (`<= 128 B`), `RunMetrics`; optional `fs`, `cache`, `process` features |
| `crates/pyrn-runner` | Rust library | Isolated cold/warm worker supervisor, container-aware `ResourceBudget`, unified TIA + CAS run pipeline, SQLite template cloning, PEP 669 + PEP 578 + `os.environ` tracer, `--cov-sysmon`, `pyrn doctor` |
| `crates/pyrn-cli` | Rust binary (`pyrn`) | Standalone CLI (`run`, `plan`, `daemon`, `cache`, `doctor`, `replay-git`) with deterministic `0`/`1`/`2` exit codes |
| `src/pyrn/` & `scripts/native_wheel.py` | Python wheel | Zero-dependency `py3-none-<platform>` binary launcher wheel (`hatch_build.py`) |
| `docs/`, `overrides/`, `zensical.toml` | Documentation | Zensical documentation site and `scripts/check_docs.py` link/anchor verifier |

## Development prerequisites

- **Rust toolchain:** stable Rust (edition 2024) with `rustfmt` and `clippy`.
- **Python environment:** Python 3.11+ (3.12+ recommended) with `pytest` and `coverage` installed
  for the live `pyrn-runner` integration tests.
- **`uv`:** used for building the zero-dependency wheel and running Zensical locally.

## Local verification commands

Run the same checks that `.github/workflows/ci.yml` executes before opening a pull request:

```sh
# 1. Formatting
cargo fmt --all -- --check

# 2. Pure in-memory pyrn-core (zero FS, SQLite/C FFI, or subprocess deps)
cargo check -p pyrn-core --no-default-features --locked
cargo clippy -p pyrn-core --no-default-features --all-targets --locked -- -D warnings
cargo test -p pyrn-core --no-default-features --locked

# 3. Workspace Clippy (zero warnings allowed)
cargo clippy --workspace --all-targets --locked -- -D warnings

# 4. Full release test suite
# Point PYRN_TEST_PYTHON at a Python 3.12+ interpreter with pytest and coverage installed
PYRN_TEST_PYTHON=/path/to/.venv/bin/python cargo test --workspace --release --locked
```

On Windows PowerShell:

```powershell
$env:PYRN_TEST_PYTHON = ".\.ci-venv\Scripts\python.exe"
cargo test --workspace --release --locked
```

## Building and smoke-testing the Python wheel

```sh
cargo build --release --locked -p pyrn-cli
PYRN_PREBUILT_BINARY=target/release/pyrn uv build --wheel
uv run --no-project --with dist/pyrn-*.whl pyrn --version
uv run --no-project --with dist/pyrn-*.whl python -m pyrn --version
uv run --no-project --with dist/pyrn-*.whl pyrn doctor --repo .
```

Remove the generated `dist/` directory once smoke checks finish.

## Building and verifying documentation locally

The documentation site is built with [Zensical](https://zensical.org/docs/get-started/) and
audited with the stdlib-only `scripts/check_docs.py`:

```sh
# Strict clean build + internal/README link & anchor check
uvx --from "zensical>=0.0.20" zensical build --strict --clean
python scripts/check_docs.py

# Live local preview (Ctrl-C to stop)
uvx --from "zensical>=0.0.20" zensical serve --dev-addr 127.0.0.1:9000
```

Delete the temporary `site/` output directory after local verification (`site/` is gitignored).
Every push to `main` builds and publishes the clean site to GitHub Pages via
`.github/workflows/docs.yml`.

## Engineering and documentation conventions

- **No `unsafe` Rust:** keep `#![forbid(unsafe_code)]` enforced across all crates.
- **Keep `pyrn-core` pure by default:** any filesystem, SQLite, or subprocess dependency must stay
  gated behind its Cargo feature (`fs`, `cache`, `process`).
- **Zero repo-specific hardcoding:** engine rules must remain generic and driven by configuration
  (`.pyrn.toml` / `pyproject.toml`), never hardcoded repository or fixture names.
- **Keep docs synchronized with code:** update the owning reference page under `docs/` whenever
  CLI flags, config keys, receipt schemas, or public Rust APIs change, and summarize user-visible
  changes under `## [Unreleased]` in [Changelog](changelog.md).
