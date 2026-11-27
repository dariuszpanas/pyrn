# Contributing to `pyrn`

Thank you for contributing to `pyrn`. The full contributor guide is published in the
[Contributing documentation](https://dariuszpanas.github.io/pyrn/contributing.html)
([docs/contributing.md](docs/contributing.md)).

## Workspace Overview

| Path | Responsibility |
| --- | --- |
| `crates/pyrn-core` | Pure-Rust in-memory TIA, symbol-level AST diff, BLAKE3 cache keys, config loader, receipts, optional `fs`/`cache`/`process` features |
| `crates/pyrn-runner` | Cold and persistent warm-pool worker supervisor, container-aware resource budgeter, SQLite template cloning, tracing, `--cov-sysmon`, `pyrn doctor` |
| `crates/pyrn-cli` | Standalone `pyrn` CLI (`run`, `plan`, `daemon`, `cache`, `doctor`, `replay-git`) |
| `src/pyrn/` & `scripts/native_wheel.py` | Zero-dependency `py3-none-<platform>` Python launcher wheel |
| `docs/` & `zensical.toml` | Zensical documentation site and `scripts/check_docs.py` link verifier |

## Local Verification Gate

Prerequisites: Rust stable toolchain (edition 2024, `rustfmt`, `clippy`), Python 3.11+ (3.12+
recommended with `pytest` and `coverage`), and `uv`.

```sh
# 1. Formatting and pure in-memory pyrn-core build (zero FS/SQLite/subprocess/FFI)
cargo fmt --all -- --check
cargo check -p pyrn-core --no-default-features --locked
cargo clippy -p pyrn-core --no-default-features --all-targets --locked -- -D warnings
cargo test -p pyrn-core --no-default-features --locked

# 2. Full workspace lint and release test suite
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --release --locked

# 3. Documentation strict build and link/anchor audit
uvx --from "zensical>=0.0.20" zensical build --strict --clean
python scripts/check_docs.py
```

Set `PYRN_TEST_PYTHON` to a Python 3.12+ interpreter with `pytest` and `coverage` installed when
running the live `pyrn-runner` E2E integration tests. Remove the generated `site/` and `dist/`
directories when finished with local checks.

## Engineering Rules

- Preserve `#![forbid(unsafe_code)]` across all crates in the workspace.
- Keep `pyrn-core` compiling and passing tests with `--no-default-features` (zero filesystem,
  SQLite/C FFI, or subprocess dependencies).
- Never hardcode project-specific fixture or repository names inside `pyrn-core` or `pyrn-runner`.
- Update the relevant reference page under `docs/` and record user-visible changes under
  `## [Unreleased]` in [docs/changelog.md](docs/changelog.md) when changing CLI, config, receipt,
  or library behavior.
