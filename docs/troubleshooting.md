# Troubleshooting

Start with the exit code and `pyrn doctor` output. Most unexpected results come from environment
resolution, cache/daemon scoping, or fail-closed TIA fallbacks.

## Read the exit code first

| Exit code | Meaning | Typical response |
| --- | --- | --- |
| `0` | All selected tests passed (or passed on rerun as `flaky`), or informational command succeeded | Continue the workflow; inspect `tests_flaky` if reruns are enabled |
| `1` | Deterministic test failure, timeout, crash (`failed` / `timed_out`), or stopped daemon status | Inspect failure summaries, `CrashCapsule`, or start the daemon |
| `2` | Invalid CLI usage, configuration error, or operational/runtime error | Fix flags, Python interpreter path, Git ref, or cache permissions |

When using `--json`, inspect stderr separately on exit `2`—CLI argument errors and operational
errors write diagnostics to stderr.

## Run `pyrn doctor` first

`pyrn doctor` verifies the interpreter, `pytest`/`coverage` availability, `sys.monitoring`
support, container/host worker budget, discovered `testpaths`, cache paths, and warm daemon state
in one call:

```sh
pyrn doctor --repo .
pyrn doctor --repo . --json
```

If `pyrn` picked the wrong Python interpreter, pass `--python /path/to/python` or set
`PYRN_PYTHON`. By default, `pyrn` checks `.venv`, `venv`, and `VIRTUAL_ENV` inside `--repo` before
falling back to `python` (Windows) or `python3` (POSIX).

## Windows `MAX_PATH` (260-character) errors

Tests that create deeply nested `tmp_path` directories or SHA-256 filenames can hit Windows'
260-character path limit under `pytest-xdist`.

`pyrn` mitigates this automatically in two ways:

1. Ephemeral worker sandboxes are created under a compact short root
   (`<TEMP>/pr/w<slot>-<hex>`, typically under 48 characters) rather than long nested project
   directories.
2. Canonicalized repository and cache paths strip Windows verbatim `\\?\` prefixes
   (`clean_canonical_path`) so Python, Git, and child processes receive standard native paths.

If you still see path-length failures on Windows:

- Check whether `--cache-dir` or `PYRN_CACHE_DIR` points to a deeply nested path; move it to a
  short directory such as `C:\pyrn-cache`.
- Enable Windows Long Paths (`LongPathsEnabled=1`) if the repository checkout itself sits deep in
  the directory tree.

## Warm daemon invalidation and stale state

The warm daemon (`pyrn daemon start`) tracks repository state automatically:

- **Source, `conftest.py`, config, or migration edits:** `pyrn` computes a BLAKE3 `core_epoch` and
  `migrations_hash` before every warm run and transparently recycles the entire worker pool
  whenever either changes.
- **Test-only edits or deletions:** including same-second, same-size edits, `pyrn` broadcasts
  `Invalidate` to every worker (busy or idle) before dispatch; any worker that fails to acknowledge
  is killed and respawned automatically.

If `pyrn run --daemon` reports `no active warm daemon found` or connection refused:

1. Check `--role`, `--python`, and `--cache-dir`: daemons are isolated per
   `(role, project_fp, runtime_fp)`. A daemon started with `--role agent` will not serve a
   `--role verify` run.
2. Check status and restart cleanly:

```sh
pyrn daemon status --repo . --role agent
pyrn daemon stop --repo . --role agent
pyrn daemon start --repo . -w auto --role agent
```

## `DJANGO_SETTINGS_MODULE` cache misses or wrong settings

By default, `pyrn` includes `DJANGO_*` and `RAY_*` environment variables (via
`env_allowlist_prefixes`) and `DJANGO_SETTINGS_MODULE` in `env_fingerprint` and CAS cache keys,
and preserves `DJANGO_SETTINGS_MODULE` across dynamic trace re-keying.

- Ensure `DJANGO_SETTINGS_MODULE` is set consistently between `pyrn daemon start` and
  `pyrn run --daemon`.
- If a test reads custom environment variables outside `DJANGO_*`/`RAY_*`, either enable `--trace`
  (automatic `os.environ` read proxy) or add their prefixes to `[tia].env_allowlist_prefixes` in
  `.pyrn.toml`.
- To inspect or clear stale entries for one role:

```sh
pyrn cache status --repo . --role agent
pyrn cache prune --repo . --role agent --only-role --max-age-days 0
```

## Python 3.12+ `sys.monitoring` and `--cov-sysmon`

- `--cov-sysmon` uses `COVERAGE_CORE=sysmon`, which requires **Python 3.12+** and the `coverage`
  package installed in the target environment.
- Run `pyrn doctor --repo .` and confirm `sys.monitoring=true` and a valid `coverage=` version.
- On Python 3.11, `sys.monitoring` is unavailable (`sys.monitoring=false`); upgrade the test
  interpreter to Python 3.12+ to use `--cov-sysmon` or PEP 669 dynamic tracing.
- When `--cov-out` is omitted, `pyrn` writes the merged `.coverage` file inside its cache directory
  so your repository stays clean. Pass `--cov-out .coverage` to write to the repository root.

## Inherited `unittest.TestCase` and dynamically generated tests

Static AST parsers cannot always enumerate test methods inherited from base classes or generated
at collection time via `pytest_generate_tests`.

`pyrn` protects against silent test drops with two mechanisms:

1. **Zero-static file safety net:** test files discovered on disk with zero static `def test_*`
   functions are included whenever `FullVerified` triggers or whenever they are reverse-reachable
   in the dependency graph from a changed file.
2. **Strict negative CAS skip-list:** when `--cache` is active, `pyrn` passes only proven cached
   nodeids as an exclusion list (`exclude_nodeids`) and refuses to skip an entire file if its
   source may contain subclassed `Test*` classes or dynamic generators, or if prior dynamic traces
   observed uncached base nodeids.

If a dynamically generated test file still misses TIA selection, verify it matches `testpaths` and
`python_files` in `pyrn doctor --repo .`, or run with `--trace` once to record dynamic edges.

## Debugging with `--no-fast-path`, `--no-sqlite-template`, and `--no-shard-heavy`

`pyrn` enables three performance optimizations by default that can each be disabled independently
for bisecting an issue:

```sh
# Route every test through full pytest protocol (disables direct pure-function fast-path)
pyrn run --repo . --no-fast-path path/to/test_file.py

# Disable :memory: and file-backed SQLite migration template cloning
pyrn run --repo . --no-sqlite-template path/to/test_file.py

# Disable heavy-file sharding in the LPT scheduler
pyrn run --repo . --no-shard-heavy path/to/test_file.py
```

The hybrid direct fast-path only accelerates provably pure zero-fixture functions (plus opt-in
`pure_autouse_guards` listed in `.pyrn.toml`). If a custom pytest plugin hook or autouse fixture
behaves unexpectedly on a test, re-run with `--no-fast-path` and remove any non-pure fixture from
`pure_autouse_guards`.

## Unexpected full-suite TIA fallbacks

When `pyrn plan` or `pyrn run --tia` selects the entire suite unexpectedly, inspect the fallback
reasons directly:

```sh
pyrn plan --repo . --base HEAD --json
```

Look at `impact.full_fallback_reasons`. Common fail-closed triggers include:

- Changes to global configuration (`pyproject.toml`, `.pyrn.toml`, lockfiles, or root
  `conftest.py`).
- Changes to non-Python files without a mapped static/dynamic asset edge under the default
  `unknown_path_policy = "full"`.
- Passing neither `--base` nor `--changed` to `pyrn plan`.

See [Test Impact Analysis](tia.md) for the complete list of fail-closed triggers and tier rules.

## Container CPU throttling or OOMKill (`exit 137`)

If workers are killed by the kernel OOM killer inside Docker or Kubernetes pods:

1. Run `pyrn doctor --repo . --json` inside the pod to verify detected `cgroup_cpu_limit`,
   `cpuset_limit`, and `cgroup_mem_limit_mb`.
2. Increase `[runner].worker_mem_mb` in `.pyrn.toml` (default `300` MiB) or export
   `PYRN_WORKER_MEM_MB=600` to match your actual per-worker RSS footprint.
3. Alternatively set a hard ceiling via `PYRN_MAX_WORKERS` or `[runner].max_workers`.

See [Resource budgeting](resource-budgeting.md) for the full budgeting algorithm.
