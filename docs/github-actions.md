# GitHub Actions integration

`pyrn` ships a first-party composite GitHub Action at the root of `dariuszpanas/pyrn` (`action.yml`) so any repository can run `pyrn run`, `pyrn plan`, `pyrn doctor`, or `pyrn config` without bootstrapping Python tools or compiling Rust in CI.

## What the action does

1. **Verifies a standalone native release binary**: downloads the matching static binary archive (`x86_64` / `aarch64` across Linux musl, macOS, and Windows) from GitHub Releases and verifies its SHA-256 digest against `pyrn-v<version>-sha256.txt`.
2. **Caches the binary in `$RUNNER_TEMP`**: multiple `pyrn` steps inside the same job reuse the already verified binary with zero extra downloads and prepend it to `$GITHUB_PATH`.
3. **Executes safely without shell injection**: all inputs pass through environment variables and bash arrays (never `eval`), and ambient `GITHUB_TOKEN` / `GH_TOKEN` are unset during test execution.
4. **Publishes structured outputs and step summaries**: reads the bounded `<= 2,048 B` `CompactReceipt` to populate `$GITHUB_OUTPUT` (`exit-code`, `verdict`, `passed`, `failed`, `cached`, `skipped`, `wall-ms`, `fast-path-pct`, `receipt-path`, `coverage-path`) and appends a concise Markdown summary table to `$GITHUB_STEP_SUMMARY`.

## Basic CI workflow with `--cov-sysmon`

Pin to a full commit SHA (recommended for supply-chain hygiene) or a release tag:

```yaml
- name: Run pytest suite via pyrn
  id: pyrn
  uses: dariuszpanas/pyrn@v0.2.2
  with:
    command: run
    workers: "4"
    role: verify
    tier: full
    cov-sysmon: "true"
    cov-out: .coverage
    metrics-kv: "true"
    receipt-out: artifacts/pyrn-receipt.json
```

!!! tip "Worker sizing on hosted 4-vCPU runners"
    Standard hosted `ubuntu-latest` runners expose 2 physical / 4 logical cores (`physical=2, logical=4`). Passing `workers: "4"` utilizes all 4 vCPUs on hosted CI runners, while leaving `workers: auto` uses the physical-core anti-SMT + cgroup budget ideal for bare-metal workstations and Kubernetes pods.

## TIA + CAS cache workflow

Combine `actions/cache@v4` with `tia: "true"` and `cache: "true"` to skip unaffected tests and serve unchanged passing test closures in milliseconds:

```yaml
- name: Restore pyrn CAS cache
  uses: actions/cache@v4
  with:
    path: ${{ runner.temp }}/pyrn-cache
    key: pyrn-cas-${{ runner.os }}-py312-${{ hashFiles('uv.lock') }}-${{ github.sha }}
    restore-keys: |
      pyrn-cas-${{ runner.os }}-py312-${{ hashFiles('uv.lock') }}-

- name: Run impacted tests with CAS cache
  uses: dariuszpanas/pyrn@v0.2.2
  with:
    command: run
    tia: "true"
    base: origin/${{ github.base_ref }}
    cache: "true"
    cache-dir: ${{ runner.temp }}/pyrn-cache
    workers: "4"
    role: verify
    tier: strict
```

See the copy-paste templates under [`examples/`](https://github.com/dariuszpanas/pyrn/tree/main/examples):

- `examples/ci-pytest-coverage.yml` — full-suite run with `--cov-sysmon` and coverage threshold check
- `examples/tia-shadow.yml` — PR Test Impact Analysis preview (`plan`) + cached execution (`run`)
- `examples/pyrn.toml` — starter `.pyrn.toml` configuration

## Action inputs

| Input | Default | Description |
| --- | --- | --- |
| `command` | `run` | Subcommand to invoke: `run`, `plan`, `doctor`, or `config` (`config show`). |
| `repo` | `.` | Target repository root directory. |
| `python` | `""` | Python interpreter executable (auto-detects `.venv` when omitted). |
| `workers` | `auto` | Worker count for `run`: `auto` or positive integer (`>= 1`). |
| `role` | `verify` | Cache/isolation role (`verify` or `agent`). |
| `tier` | `strict` | TIA confidence tier (`strict`, `transitive`, or `full`). |
| `markers` | `""` | Pytest marker expression (`-m`) for `run`. |
| `keyword` | `""` | Pytest keyword expression (`-k`) for `run`. |
| `config-file` | `""` | Explicit pytest configuration file (`-c`) for `run`. |
| `targets` | `""` | Space- or newline-separated test files or directories for `run`. |
| `cov-sysmon` | `"false"` | Enable built-in Python 3.12+ `COVERAGE_CORE=sysmon` capture and shard merge. |
| `cov-out` | `""` | Destination path for merged `--cov-sysmon` `.coverage` file. |
| `tia` | `"false"` | Enable symbol-level Test Impact Analysis for `run`. |
| `base` | `""` | Git base ref for TIA symbol diffing (e.g., `origin/main`). |
| `changed` | `""` | Comma-separated explicit changed repository-relative paths. |
| `cache` | `"false"` | Enable role-segregated SQLite WAL CAS test cache for `run`. |
| `cache-dir` | `""` | Override base cache directory (pair with `actions/cache`). |
| `reruns` | `"0"` | Auto-rerun failing tests up to `N` times (`ExecVerdict::Flaky`). |
| `test-timeout` | `"60"` | Hard per-test watchdog timeout in seconds. |
| `metrics-kv` | `"true"` | Emit `PYRN_METRIC key=value` line to stdout. |
| `metrics-prefix` | `""` | Custom prefix for `--metrics-kv` output. |
| `receipt-out` | `""` | Path to persist the bounded `<= 2,048 B` `CompactReceipt` JSON. |
| `json-out` | `""` | Path to persist the full `RunReport` JSON. |
| `plain` | `"false"` | Disable ANSI colors and Unicode box borders (`--plain`). |
| `extra-args` | `""` | Whitespace-separated extra pytest arguments forwarded after `--`. |
| `version` | `"0.2.2"` | Verified release version of `pyrn` to install. |
| `step-summary` | `"true"` | Append a structured Markdown summary table to `$GITHUB_STEP_SUMMARY`. |

## Action outputs

| Output | Description |
| --- | --- |
| `exit-code` | Exit code (`0`, `1`, or `2`). |
| `verdict` | Receipt verdict (`pass`, `flaky`, `failed`, `timed_out`, `config_error`). |
| `passed` | Number of passed tests. |
| `failed` | Number of failed, errored, crashed, or timed-out tests. |
| `cached` | Number of tests served from CAS cache. |
| `skipped` | Number of skipped tests. |
| `wall-ms` | End-to-end wall-clock runtime in milliseconds. |
| `fast-path-pct` | Percentage of executed tests handled by the direct fast-path. |
| `receipt-path` | Persisted `CompactReceipt` path when `receipt-out` is set. |
| `coverage-path` | Merged coverage path when `cov-out` is set. |

## Related guides

- [Assess and adopt in CI](ci-adoption.md) — staged rollout strategy from `pytest-xdist`.
- [Commands](commands.md) — complete CLI subcommand and flag reference.
- [Configuration](configuration.md) — `.pyrn.toml` and `pyproject.toml` schema.
- [Receipts and telemetry](receipts-and-telemetry.md) — `CompactReceipt` and `--metrics-kv` schemas.
