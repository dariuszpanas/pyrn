# Recipes

Copy-paste configurations and command lines for common `pyrn` workflows. Every flag below matches
the current CLI contract; see [Commands](commands.md) and [Configuration](configuration.md) for the
complete flag and schema references.

## Local warm-daemon TDD loop

Keep a persistent worker pool warm in one terminal while running sub-second incremental TIA + CAS
checks in another:

```sh
# Terminal 1: boot warm pool sized to physical cores
pyrn daemon start --repo . -w auto --role agent
```

```sh
# Terminal 2: run only tests impacted since HEAD, serving unchanged passes from CAS cache
pyrn run --repo . --tia --base HEAD --cache --daemon --role agent

# Filter to a single area while keeping the warm pool
pyrn run --repo . --daemon --role agent -k "billing" tests/unit/

# Inspect daemon worker lanes, code epoch, and runs served
pyrn daemon status --repo .

# Stop cleanly when finished
pyrn daemon stop --repo .
```

The daemon automatically recycles workers when source modules, `conftest.py`, config files, or
migrations change, and broadcasts `Invalidate` when only test files are modified or deleted.

## Django and SQLite test suites

`pyrn` automatically clones BLAKE3-keyed SQLite migration templates for both `:memory:` Django test
databases (`sqlite3.backup()`) and fresh file-backed SQLite databases, and hashes `DJANGO_*`
environment variables (including `DJANGO_SETTINGS_MODULE`) into cache keys.

Add a `.pyrn.toml` at your repository root when you want to tune Django discovery, extra env
prefixes, or verified opt-in autouse guards:

```toml
testpaths = ["tests"]
python_files = ["test_*.py", "*_test.py"]

[tia]
symbol_level_tia = true
conftest_policy = "smart_fixture_scoped"
unknown_path_policy = "full"
env_allowlist_prefixes = ["DJANGO_", "RAY_", "APP_FEATURE_"]
extra_full_triggers = ["config/routes.yaml"]

[runner]
worker_mem_mb = 350
# Optional: only list project autouse fixtures proven to be pure/DB-conditional guards
pure_autouse_guards = []
```

Run with an explicit settings module:

```sh
DJANGO_SETTINGS_MODULE=myproject.settings.test \
  pyrn run --repo . --tia --base HEAD --cache -w auto
```

To bypass SQLite template cloning while debugging a custom migration hook, pass
`--no-sqlite-template`:

```sh
pyrn run --repo . -w auto --no-sqlite-template tests/integration/test_migrations.py
```

## GitHub Actions CI workflow

Use `fetch-depth: 0` so `--base origin/main` diffs against the real merge base without network
fetches, cache `.pyrn-cache` between runs, and emit bounded receipts and KV metrics:

```yaml
name: Tests

on:
  pull_request:
  push:
    branches: [main]

permissions:
  contents: read

jobs:
  pytest:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
          persist-credentials: false

      - uses: astral-sh/setup-uv@v6
        with:
          python-version: "3.12"

      - name: Install project dependencies
        run: uv sync --frozen

      - name: Restore pyrn CAS cache
        uses: actions/cache@v4
        with:
          path: .pyrn-cache
          key: pyrn-${{ runner.os }}-py312-${{ hashFiles('uv.lock', 'pyproject.toml') }}
          restore-keys: |
            pyrn-${{ runner.os }}-py312-

      - name: Run impacted tests on PRs (full suite on main)
        run: |
          if [ "${{ github.event_name }}" = "pull_request" ]; then
            pyrn run --repo . \
              --tia --base origin/main \
              --cache --cache-dir .pyrn-cache \
              --role verify --tier strict \
              -w auto \
              --metrics-kv \
              --receipt-out receipt.json
          else
            pyrn run --repo . \
              --cache --cache-dir .pyrn-cache \
              --role verify --tier full \
              -w auto \
              --metrics-kv \
              --receipt-out receipt.json
          fi

      - name: Prune aged cache entries
        if: always()
        run: pyrn cache prune --repo . --cache-dir .pyrn-cache --max-age-days 30
```

## Docker and Kubernetes ARC runner pods

Inside container pods with strict cgroup CPU/memory limits, verify the resolved budget with
`pyrn doctor` and configure `PYRN_WORKER_MEM_MB` (or `[runner].worker_mem_mb`) to match your
application's resident worker footprint:

```sh
# Inspect detected cgroup v2/v1 CPU quota, cpuset, and memory OOM cap
pyrn doctor --repo . --json

# Run with memory-aware auto-sizing (e.g. 512 MiB/worker budget) and an explicit cache volume
PYRN_WORKER_MEM_MB=512 \
  pyrn run --repo . \
    -w auto \
    --cache --cache-dir /var/cache/pyrn \
    --role verify \
    --test-timeout 90
```

To enforce a hard upper bound on shared runners regardless of host cores:

```sh
PYRN_MAX_WORKERS=4 pyrn run --repo . -w auto
```

See [Resource budgeting](resource-budgeting.md) for how cgroup limits and overrides interact.

## Fast Python 3.12+ coverage with `--cov-sysmon`

`pyrn` sets `COVERAGE_CORE=sysmon` per worker, writes isolated `.coverage.<worker>` shards into an
ephemeral sandbox, and combines them automatically via `python -m coverage combine` at the end of
the run:

```sh
pyrn run --repo . -w auto --cov-sysmon --cov-out .coverage
python -m coverage report --show-missing
```

When `--cov-out` is omitted, the merged coverage file is written inside `pyrn`'s cache directory
so your working tree stays clean. Confirm `sys.monitoring=true` and `coverage` presence beforehand
with `pyrn doctor --repo .`.

## Custom CI telemetry (`--metrics-kv`, `--metrics-prefix`, `--ndjson`)

By default, `--metrics-kv` prints a single deterministic key-value metrics line prefixed with
`PYRN_METRIC`. Override the prefix via CLI flag or `PYRN_METRICS_PREFIX` to integrate with any
existing CI log parser:

```sh
# Explicit CLI prefix
pyrn run --repo . --tia --base HEAD --role verify --metrics-kv --metrics-prefix CI_METRIC

# Equivalent environment variable
PYRN_METRICS_PREFIX=CI_METRIC pyrn plan --repo . --base HEAD --metrics-kv
```

For real-time progress consumers, stream structured NDJSON events while still writing a bounded
`<= 2,048 B` receipt and full report JSON:

```sh
pyrn run --repo . -w auto \
  --ndjson \
  --receipt-out artifacts/receipt.json \
  --json-out artifacts/report.json
```

See [Receipts and telemetry](receipts-and-telemetry.md) for the exact schemas and key catalog.

## Flake detection and crash/timeout resilience

Guard CI against transient flakes and hung or segfaulting C extensions:

```sh
pyrn run --repo . -w auto --reruns 2 --test-timeout 45 --receipt-out receipt.json
```

- Tests that fail initially and pass on a rerun are reported as `flaky` (`ExecVerdict::Flaky`,
  exit `0` when no deterministic failures remain) and are **never** seeded into the CAS cache.
- Hung tests exceeding `--test-timeout` or hard worker crashes trigger worker termination, respawn,
  and re-queue of all remaining unit items without losing sibling test results.

## Read-only Git commit history TIA replay

Audit TIA precision and speedup across recent Git history without modifying the repository or
executing tests:

```sh
# Replay the last 30 non-merge commits
pyrn replay-git --repo . --limit 30 --tier strict

# Include per-file timing weights and dump JSON summary
pyrn replay-git --repo . --limit 50 --tier strict --durations-csv per_file_timings.csv --json
```
