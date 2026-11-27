# Receipts and telemetry

`pyrn` emits deterministic, size-bounded, machine-readable outputs designed for CI gates, observability pipelines, and automated tooling. All shared contract types live in `crates/pyrn-core/src/receipt.rs` and are formatted by `crates/pyrn-runner/src/diagnostics.rs`.

## Output surface summary

| Flag | Destination | Schema / Format | Size bound | Primary consumer |
| --- | --- | --- | --- | --- |
| `--receipt-out <PATH>` | File | `CompactReceipt` (`pyrn.receipt.v1`) | `<= 2,048 B` hard ceiling | Gate evaluators, status checks, audit logs |
| `--json` | `stdout` | `pyrn.v1` JSON envelope | Unbounded (compact envelope) | Scripts and CI steps reading structured output |
| `--json-out <PATH>` | File (`pyrn run` only) | Pretty `RunReport` JSON (includes every `TestRecord`, `WorkerStats`, and optional `traces`) | Unbounded | Post-run analytics, JUnit converters, slow-test reports |
| `--ndjson` | `stdout` (`pyrn run` only) | One `StreamEvent` JSON object per line | Per-event line | Live progress UIs and streaming log parsers |
| `--metrics-kv` | `stdout` (`run`, `plan`) | Single `<PREFIX> key=<i64> ...` line | Fixed 22 integer fields | Regex/awk CI metric scrapers |

## `CompactReceipt` (`pyrn.receipt.v1`, `<= 2,048 B`)

`CompactReceipt` provides a self-contained verification summary guaranteed to serialize within `MAX_RECEIPT_BYTES = 2,048` bytes via `CompactReceipt::to_bounded_json()`.

### Fields

| Field | Type | Description |
| --- | --- | --- |
| `schema` | `string` | Always `"pyrn.receipt.v1"` (`CompactReceipt::SCHEMA_V1`). |
| `verdict` | `ExecVerdict` | `"pass"`, `"failed"`, `"timed_out"`, `"flaky"`, or `"config_error"`. |
| `tier` | `ConfidenceTier` | Effective tier: `"strict_impacted"`, `"transitive_impacted"`, or `"full_verified"`. |
| `role` | `SlotRole` | Isolation role: `"agent"` or `"verify"`. |
| `env_fingerprint` | `string` | Leading 16 hex characters of the environment BLAKE3 fingerprint. |
| `wall_ms` | `u64` | Total wall-clock duration in milliseconds. |
| `tests_total` | `usize` | Total tests (`total_collected + tests_cached` for `run`; total repo test nodes for `plan`). |
| `tests_selected` | `usize` | Tests selected after TIA/target filtering. |
| `tests_cached` | `usize` | Tests served from `CasTestCache` (or skipped as unchanged in `plan`). |
| `tests_tainted` | `usize` | Selected tests classified as `CacheSafety::TaintedAlwaysRun`. |
| `tests_passed` | `usize` | Executed tests with outcome `passed`. |
| `tests_failed` | `usize` | Sum of `failed + errors + timed_out + crashed` in `run` (`0` in `plan`). |
| `tests_skipped` | `usize` | Executed tests with outcome `skipped`. |
| `tests_flaky` | `usize` | Tests that initially failed and passed on `--reruns`. |
| `worker_restarts` | `u32` | Total worker respawns caused by crashes or watchdog timeouts. |
| `fast_path_tests` | `usize` | Executed tests served by the hybrid direct fast-path. |
| `crash_capsule` | `CrashCapsule` (omitted when `null`) | Bounded first-failure witness when a test failure or collection error occurred. |
| `summary` | `string` | One-line human summary (`verdict mode=... tier=... workers=... wall=...ms`). |

### Hard byte-ceiling enforcement

`to_bounded_json()` truncates `env_fingerprint` to 16 characters and serializes compact JSON. If the resulting byte length exceeds `2,048` bytes, it safely truncates `summary` to 96 UTF-8 bytes and `crash_capsule.summary` to 64 UTF-8 bytes (never splitting a multi-byte UTF-8 codepoint) before re-serializing.

### Example `CompactReceipt`

```json
{
  "schema": "pyrn.receipt.v1",
  "verdict": "failed",
  "tier": "strict_impacted",
  "role": "verify",
  "env_fingerprint": "9f2a4c1d8b7e3051",
  "wall_ms": 1842,
  "tests_total": 120,
  "tests_selected": 120,
  "tests_cached": 84,
  "tests_tainted": 2,
  "tests_passed": 35,
  "tests_failed": 1,
  "tests_skipped": 0,
  "tests_flaky": 0,
  "worker_restarts": 0,
  "fast_path_tests": 24,
  "crash_capsule": {
    "witness_digest": "3a7e1f4d9c0b2e6a8d5f1c3b7a9e0d2f4c6b8a1e3d5f7c9a0b2e4d6f8a1c3e5b",
    "gate_name": "pyrn",
    "summary": "tests/unit/test_billing.py::test_tax_rounding :: E   AssertionError: assert 10.01 == 10.00"
  },
  "summary": "failed mode=cold tier=strict_impacted workers=4(cgroup_cpu_quota) passed=35 cached=84 failed=1 skipped=0 flaky=0 wall=1842ms"
}
```

## `CrashCapsule` (`<= 128 B` summary)

When a run encounters at least one failing test (`TestOutcome::is_failure()`) or collection error, `build_compact_receipt` populates `crash_capsule` from the first failing test (or the first collection error if no individual test failed):

| Field | Type | Semantics |
| --- | --- | --- |
| `witness_digest` | `string` | 64-hex BLAKE3 hash of the full raw failure detail (or `nodeid` if detail is empty). |
| `gate_name` | `string` | `"pyrn"` for a test failure, `"pyrn-collect"` for a collection error. |
| `summary` | `string` | `"<nodeid> :: <first_useful_line>"`, UTF-8-safely truncated to `<= 128` bytes (`MAX_CRASH_CAPSULE_BYTES`). |

`first_useful_line` scans the raw traceback/output in order and selects:

1. The first trimmed line starting with `E   ` or containing `AssertionError`, `Error:`, `timed out`, or `crashed`.
2. Otherwise, the first non-empty trimmed line.
3. Otherwise, `"test failure"`.

## `ExecVerdict` and `TestOutcome` contracts

### `ExecVerdict`

`compute_verdict` evaluates counts in strict precedence order:

| Precedence | `ExecVerdict` | Condition | CLI exit code |
| --- | --- | --- | --- |
| 1 | `timed_out` | `timed_out > 0` | `1` |
| 2 | `failed` | `failed > 0 \|\| errors > 0 \|\| crashed > 0 \|\| !collection_errors.is_empty()` | `1` |
| 3 | `flaky` | `flaky > 0` (all failures recovered on `--reruns`) | `0` |
| 4 | `pass` | No failures, timeouts, collection errors, or flakes | `0` |
| — | `config_error` | Configuration or runtime setup error | `2` |

### `TestOutcome`

| Variant | `is_failure()` | Meaning |
| --- | --- | --- |
| `passed` | `false` | Test executed and passed. |
| `failed` | `true` | Assertion or runtime exception during test call. |
| `skipped` | `false` | Test skipped via `pytest.skip` or skip marker. |
| `xfailed` | `false` | Expected failure (`pytest.xfail` / `@pytest.mark.xfail`). |
| `xpassed` | `false` | Unexpectedly passed an `xfail` test (non-strict). |
| `error` | `true` | Fixture setup or teardown failure. |
| `timed_out` | `true` | Active test exceeded `--test-timeout` hard watchdog. |
| `crashed` | `true` | Worker process exited abnormally (`SIGSEGV`, `os._exit`, EOF) while running this test. |
| `flaky` | `false` | Failed initially, then passed on `--reruns`. |
| `cached` | `false` | Verified passing hit served from `CasTestCache` without execution. |

## NDJSON `StreamEvent` schemas (`--ndjson`)

Each line emitted to `stdout` under `pyrn run --ndjson` is a JSON object tagged with `"type"`:

### `run_started`

```json
{
  "type": "run_started",
  "mode": "cold",
  "role": "verify",
  "tier": "strict_impacted",
  "project_root": "/workspace/myservice",
  "workers": 4,
  "files_discovered": 18,
  "units_scheduled": 22,
  "tests_cached": 0
}
```

### `test_finished`

```json
{
  "type": "test_finished",
  "nodeid": "tests/unit/test_billing.py::test_total",
  "outcome": "passed",
  "duration_ms": 1.42,
  "worker_id": "w0-hermetic",
  "fast_path": true
}
```

### `worker_fault`

Emitted when a worker crashes or exceeds the watchdog timeout and remaining tests in the unit are requeued:

```json
{
  "type": "worker_fault",
  "worker_id": "w1-hermetic",
  "kind": "timed_out",
  "nodeid": "tests/unit/test_hang.py::test_deadlock",
  "requeued": 7
}
```

`kind` is `"timed_out"` or `"crashed"`; `nodeid` is `null` if the fault occurred during unit collection before any `test_start`.

### `run_finished`

```json
{
  "type": "run_finished",
  "verdict": "pass",
  "exit_code": 0,
  "wall_ms": 1842,
  "passed": 120,
  "cached": 0,
  "failed": 0,
  "skipped": 2,
  "flaky": 0,
  "timed_out": 0,
  "crashed": 0
}
```

!!! note "Live streaming vs report replay"
    Uncached cold runs stream `run_started`, `test_finished`, `worker_fault`, and `run_finished` live as events happen. Warm-daemon runs and runs with `tests_cached > 0` replay `run_started`, sorted `test_finished` records, and `run_finished` from the completed `RunReport` (so `worker_fault` events are only emitted during live uncached cold runs).

## `--metrics-kv` key catalog

`--metrics-kv` prints a single space-delimited line of 22 signed 64-bit integers (`i64`) prefixed by `--metrics-prefix` (or `PYRN_METRICS_PREFIX`, defaulting to `PYRN_METRIC`):

```text
PYRN_METRIC tests_total=120 tests_selected=120 tests_cached=84 tests_tainted=2 tests_passed=36 tests_failed=0 tests_skipped=0 tests_xfailed=0 tests_xpassed=0 tests_flaky=0 tests_timed_out=0 tests_crashed=0 fast_path_tests=24 workers_selected=4 worker_restarts=0 db_template_hits=3 cache_hit_pct=70 tia_parse_ms=112 tia_graph_ms=19 wall_ms=640 discovery_ms=8 pool_ready_ms=195
```

Every key in exact emission order:

| Key | Type | Meaning in `pyrn run` | Meaning in `pyrn plan` |
| --- | --- | --- | --- |
| `tests_total` | `i64` | `total_collected + tests_cached` | Total discovered test nodes in repo |
| `tests_selected` | `i64` | Tests selected for this run | Impacted test nodes selected by TIA |
| `tests_cached` | `i64` | Verified CAS hits skipped | Unchanged test nodes (`total - selected`) |
| `tests_tainted` | `i64` | Selected tests marked `TaintedAlwaysRun` | Impacted tests marked `TaintedAlwaysRun` |
| `tests_passed` | `i64` | Executed tests with outcome `passed` | `0` |
| `tests_failed` | `i64` | `failed + errors + timed_out + crashed` | `0` |
| `tests_skipped` | `i64` | Executed tests with outcome `skipped` | `0` |
| `tests_xfailed` | `i64` | Executed tests with outcome `xfailed` | `0` |
| `tests_xpassed` | `i64` | Executed tests with outcome `xpassed` | `0` |
| `tests_flaky` | `i64` | Tests reclassified as `flaky` after `--reruns` | `0` |
| `tests_timed_out` | `i64` | Tests killed by `--test-timeout` watchdog | `0` |
| `tests_crashed` | `i64` | Tests whose worker crashed unexpectedly | `0` |
| `fast_path_tests` | `i64` | Tests executed via hybrid direct fast-path | `0` |
| `workers_selected` | `i64` | Effective workers allocated (`0` on 100% CAS hit) | `0` |
| `worker_restarts` | `i64` | Total worker respawns across slots | `0` |
| `db_template_hits` | `i64` | SQLite `:memory:` + file-backed template clones | `0` |
| `cache_hit_pct` | `i64` | Integer percent `(tests_cached * 100) / max(tests_selected, tests_total)` | `0` |
| `tia_parse_ms` | `i64` | Parallel Rust AST parse + BLAKE3 hash time (ms) | Parallel Rust AST parse + BLAKE3 hash time (ms) |
| `tia_graph_ms` | `i64` | Dependency graph build time (ms) | Dependency graph build time (ms) |
| `wall_ms` | `i64` | End-to-end run wall-clock time (ms) | `0` (plan wall time is in `receipt.wall_ms`) |
| `discovery_ms` | `i64` | Static test file discovery time (ms) | `0` |
| `pool_ready_ms` | `i64` | Worker startup/recycle handshake time (ms) | `0` |

## `--json` CLI envelopes

All `--json` subcommands emit a versioned envelope with `"schema_version": "pyrn.v1"` and `"command"`:

| Command | `"command"` value | Key top-level fields |
| --- | --- | --- |
| `pyrn run --json` | `"run"` | `ok`, `exit_code`, `receipt`, `receipt_bytes`, `cache_seeded`, `resource_budget`, `metrics_kv`, `impact` |
| `pyrn plan --json` | `"plan"` | `ok`, `env_fingerprint`, `project_fingerprint`, `parse_and_hash_ms`, `graph_build_ms`, `impact`, `selected_files`, `selected_nodeids_count`, `cache_keys_count`, `metrics_kv`, `receipt`, `receipt_bytes` |
| `pyrn daemon status --json` | `"daemon.status"` | `running`, `daemon` (when running) or `detail` (when stopped) |
| `pyrn daemon stop --json` | `"daemon.stop"` | `ok`, `response` |
| `pyrn cache status --json` | `"cache.status"` | `ok`, `cas_db_path`, `db_size_bytes`, `role`, `stats` |
| `pyrn cache prune --json` | `"cache.prune"` | `ok`, `cas_db_path`, `max_age_days`, `role_filter`, `removed`, `remaining_stats` |
| `pyrn doctor --json` | `"doctor"` | `ok`, `doctor` (`DoctorReport`), `slowest_historical_files_ms` |
| `pyrn replay-git --json` | *(direct `ReplaySummary`)* | `commits_analyzed`, `tier`, `symbol_level_tia`, `cochange_recall_pct`, `median_speedup`, `mean_reduction_pct`, `rows` |

## Related guides

- [Command reference](commands.md) — flags controlling `--receipt-out`, `--json`, `--json-out`, `--ndjson`, and `--metrics-kv`.
- [CI adoption](ci-adoption.md) — wiring receipts and KV metrics into GitHub Actions and merge queues.
- [Embedding `pyrn-core`](embedding.md) — constructing `CompactReceipt` and `RunMetrics` directly in Rust.
