# Execution engine

`pyrn-runner` (`crates/pyrn-runner`) is an async Tokio supervisor controlling a pool of isolated, persistent Python workers (`crates/pyrn-runner/python/pyrn_worker.py`) over authenticated loopback TCP NDJSON. The same execution pool powers both one-shot cold runs (`pyrn run`) and persistent warm daemon runs (`pyrn daemon start` + `pyrn run --daemon`).

## Architecture and control plane

- **Self-contained worker materialization**: `pyrn_worker.py` is embedded into the Rust binary via `include_str!` and materialized outside the target repository at `<cache_dir>/<role>/<project_fp>/<runtime_fp[:16]>/pyrn_worker_<16hex>.py` using an atomic write-and-rename.
- **Dedicated loopback TCP NDJSON channel**: The Rust supervisor binds `127.0.0.1:0` (`TCP_NODELAY`) and generates a random 128-bit hex `auth_token`. Workers connect back and authenticate with a `WorkerEvent::Hello` handshake. Test `stdout` and `stderr` are drained independently on pipes, preventing test output from ever corrupting the control protocol.
- **Supervisor commands (`WorkerCommand`)**: `run_unit`, `invalidate`, `ping`, `shutdown`.
- **Worker events (`WorkerEvent`)**: `hello`, `pong`, `invalidated`, `unit_collected`, `unit_collect_error`, `test_start`, `test_result`, `unit_done`, `worker_error`.

## Persistent live `pytest.Session` and pytest 9.2 `FixtureManager` rebinding

Each worker boots a single persistent `pytest.Config` and `pytest.Session` once at process startup:

- Launches pytest internally with `-p no:cacheprovider -p no:xdist -p no:cov --rootdir <project_root> -o addopts=`, plus `--basetemp <worker_tmp>/t`, optional `-c <config_file>`, optional `-m <markers>`, and any extra pytest args.
- Pre-loads the seed `conftest.py` directory (`tests/conftest.py` or nearest test ancestor) once during `pytest_sessionstart`, amortizing interpreter startup, `django.setup()`, and session-scoped fixtures across all subsequent work units and warm daemon invocations.
- **Session keep-alive**: A synthetic `_KeepAliveItem` (`__pyrn_session_keepalive__`) attached to `session` is passed to `session._setupstate.teardown_exact(keepalive_sentinel)` after every work unit. Function, class, and module fixtures tear down cleanly between units while session-scoped fixtures stay alive.
- **Pytest 9.2+ `FixtureManager` node-identity rebinding (`_prepare_fixturemanager_for_unit`)**: Before each incremental `session.perform_collect(args=paths, genitems=True)`, the worker prunes non-session node keys from `session._fixturemanager._node_autousenames` and `_arg2node2fixturedefs`, and repopulates `_pending_conftests` for already-loaded subdirectory `conftest.py` modules. Fresh `Directory` collector nodes created on incremental collection therefore receive exact directory-scoped fixtures without reloading modules.

## Affinity lanes and worker pool sizing

`discovery.rs` classifies every discovered test file statically into one of three `ResourceLane` values:

| Lane | Static classification rules | Worker behavior |
| --- | --- | --- |
| `hermetic` | Default when neither DB nor external markers are present. | Runs pure Python and mocked unit tests. Steals from `sqlite_django` only after the `hermetic` queue drains. |
| `sqlite_django` | File contains `django_db`, `transactional_db`, `django_db_reset_sequences`, `django_db_serialized_rollback`, `admin_client`, `admin_user`, `live_server`, `MigrationExecutor`, `TransactionTestCase`, `TestCase)`, `(TestCase`, `(db)`, `(db,`, `, db)`, `, db,`, or `db:`. | Keeps warm Django/SQLite session state together. Steals from `hermetic` only after the `sqlite_django` queue drains. |
| `serial_external` | File contains `pytest.mark.real_ray`, `pytest.mark.live_cluster`, or `pytest.mark.postgresql` (unless `-m` excludes all three with `not real_ray`, `not live_cluster`, and `not postgresql`). | Runs sequentially on a single dedicated worker (never sharded, never steals across lanes). |

Worker allocation differs by mode:

- **Cold runs (`plan_worker_split`)**: splits `effective_workers` proportionally to total estimated CPU time across `hermetic` vs `sqlite_django`, clamping both lanes to `[1, total - 1]` when both have work (and adds 1 dedicated `serial_external` worker if `serial_external` units exist).
- **Warm daemon (`serve_daemon`)**: pre-allocates `sqlite_w = max(1, (w * 2) / 5)` (or `1` when `w == 1`) `sqlite_django` workers and `w - sqlite_w` `hermetic` workers.

## LPT scheduling, heavy-file sharding, and sticky work-stealing

1. **Historical EWMA timing (`timings.json`)**:
   - Stored outside the repository at `<cache_dir>/<role>/<project_fp>/timings.json` (updated atomically after every run with `0.6 * old + 0.4 * new`).
   - When no historical timing exists for a file, `estimate_file_ms` falls back to `estimated_tests * per_test_ms + line_count * 0.08` (floor `15.0 ms`), using `per_test_ms = 1.5` (`hermetic`), `22.0` (`sqlite_django`), or `500.0` (`serial_external`).
2. **Deterministic heavy-file sharding** (disable with `--no-shard-heavy`):
   - Active when `workers >= 2`, lane is not `serial_external`, and the file has no module/package/session fixture (`scope="module|package|session"`, `def setup_module`, `def teardown_module`).
   - `max_shards = clamp(workers / 2, 2, 8)`.
   - Shard thresholds:
     - `sqlite_django`: `>= 12,000 ms` -> `max_shards`; `>= 3,000 ms` or `>= 2,500` lines -> `min(4, max_shards)`; `>= 1,500 ms` or `>= 1,200` lines -> `min(2, max_shards)`; else `1`.
     - `hermetic`: `>= 12,000 ms` -> `max_shards`; `>= 4,000 ms` or `>= 3,500` lines -> `min(4, max_shards)`; `>= 1,800 ms` or `>= 1,800` lines -> `min(2, max_shards)`; else `1`.
   - Inside workers, `_select_shard` groups contiguous `(module, cls)` items and greedily balances group sizes across shard buckets (falling back to modulo or contiguous slicing when fewer groups exist than shards), preserving class-scoped fixtures with 100% disjoint coverage.
3. **LPT ordering and sticky work-stealing**:
   - Each lane queue is sorted longest-processing-time-first so stragglers launch at `t = 0`.
   - `AffinityWorkQueue::pop_for_sticky` scans the top 16 units in the worker's preferred lane for a unit whose files are already imported in `worker.warm_files`, avoiding re-imports on warm workers before falling back to queue head and cross-lane stealing.

## SQLite migration template cloning

Enabled by default (disable with `--no-sqlite-template`), `pyrn_worker.py` accelerates Django SQLite schema creation through two hooks keyed by Rust's BLAKE3 `migrations_hash` (`pyrn.sqlite-template.v2` over `python_exe` and all `*/migrations/*.py`, `*/models.py`, `*conftest.py`, `*pyproject.toml`, `*.pyrn.toml`, `*settings.py` up to depth 6):

1. **`BaseDatabaseCreation.create_test_db` hook** (template path `<runtime_dir>/sqlite_template_<16hex>.sqlite3`):
   - **`:memory:` SQLite**: on hit (when `keepdb=False`), opens the read-only template via SQLite URI and clones into the live in-memory connection using `sqlite3.Connection.backup()`. Between units, `_reset_sqlite_from_template_if_active` rolls back and restores the active `:memory:` DB from the template in ~2–3 ms.
   - **File-backed SQLite**: removes `-wal`, `-shm`, and `-journal` sidecars and copies the template via `shutil.copyfile`.
   - **Cache miss**: runs Django's original `create_test_db`, then atomically snapshots the freshly migrated SQLite connection (`src_conn.backup(dst_conn)` -> `os.replace`) for sibling workers and future runs.
2. **`django.core.management.commands.migrate.Command.handle` hook** (template path `<stem>_file_<settings_sig>.sqlite3`):
   - Accelerates repeated fresh empty file-backed `call_command("migrate", verbosity=0)` calls (such as per-test owned SQLite fixtures).
   - Strictly gated: runs only when `app_label`, `migration_name`, `fake`, `fake_initial`, `plan`, `check_unapplied`, `prune` are unset/false, `verbosity == 0`, the target connection is a plain non-`:memory:` non-URI SQLite file with `0` rows in `sqlite_master`, and not already inside a template build.
   - Double-keyed by `migrations_hash` plus a runtime BLAKE2b digest of `(INSTALLED_APPS, MIGRATION_MODULES, run_syncdb)`.

Both hooks clear `django.contrib.contenttypes.models.ContentType.objects` cache after cloning and fall back cleanly to original Django execution on any exception.

## Per-worker sandboxing and environment sanitization

Each worker owns a `WorkerSandbox` with RAII cleanup on worker drop:

- **Compact sandbox directory**: `<OS temp>/pr/w<slot>-<4hex>` (intentionally `< 48` characters to prevent Windows 260-character `MAX_PATH` failures under pytest's `tmp_path`), passed as `TMPDIR`, `TEMP`, `TMP`, and pytest `--basetemp <tmp>/t`.
- **Disjoint port range**: each worker slot receives a deterministic 100-port block in `[20000..55000)`:
  `port_start = 20000 + (slot % 350) * 100`, `port_end = port_start + 99`, exported via `PYRN_PORT_START`, `PYRN_PORT_END`, and `DJANGO_LIVE_TEST_SERVER_ADDRESS=127.0.0.1:<start>-<end>`.
- **Stripped ambient environment variables** (removed before spawn to eliminate bleed):
  `PYTEST_CURRENT_TEST`, `PYTEST_XDIST_WORKER`, `PYTEST_XDIST_WORKER_COUNT`, `PYTEST_XDIST_TESTRUNUID`, `PYTEST_ADDOPTS`, `COVERAGE_FILE`, `COVERAGE_CORE`, `COV_CORE_SOURCE`, `COV_CORE_CONFIG`, `COV_CORE_DATAFILE`.
- **Injected sterile Python variables**:
  `PYTHONDONTWRITEBYTECODE=1`, `PYTHONFAULTHANDLER=1`, `PYTHONUNBUFFERED=1`, `PYTHONUTF8=1`, `PYRN_WORKER_ID=w<slot>-<lane>`.

## Warm daemon lifecycle and invalidation

`pyrn daemon start` writes its RPC endpoint and 128-bit `auth_token` atomically to `<cache_dir>/<role>/<project_fp>/<runtime_fp[:16]>/daemon.json`:

- **Stable `runtime_fp`**: `pyrn.runtime.v1` hashes `(OS, ARCH, python_exe)` only—so code, config, and migration edits do not break daemon discovery for `pyrn run --daemon` or `pyrn daemon stop`.
- **Content-hashed `core_epoch` auto-recycle**: Before every run, the daemon runs `scan_repo_fingerprint` (BLAKE3 content hashes up to depth 7). `core_epoch` (`pyrn.core-epoch.v2`) covers all non-test `.py` files, `conftest.py`, `pyproject.toml`, `.pyrn.toml`, `pyrn.toml`, `pytest.ini`, and `uv.lock`. If `core_epoch` changed or the pool has shrunk, the daemon shuts down all workers in parallel, refreshes `migrations_hash` and `pure_autouse_guards`, and spawns a fresh pool transparently.
- **Broadcast `Invalidate` for test-only edits**: If `core_epoch` is unchanged, `diff_changed_test_files` compares BLAKE3 content hashes across all test files—catching additions, deletions, and even same-second, same-size edits. When any test file changed, the daemon broadcasts `WorkerCommand::Invalidate { changed_paths }` to **every** worker in parallel (10-second timeout per worker; unresponsive workers are hard-killed and respawned). Workers evict matching modules from `sys.modules` and clear `session._collection_cache` and `module_safe_cache`.

## Hybrid direct fast-path and `pure_autouse_guards`

Enabled by default (disable with `--no-fast-path`), `_check_fast_path` executes provably pure zero-fixture pytest functions directly, bypassing per-item pytest hook and fixture resolution overhead while preserving full parity.

An item qualifies **only** if every conservative check passes:

1. `type(item).__name__ == "Function"` and not a `unittest.TestCase` subclass.
2. No xUnit hooks on the class (`setup_method`, `teardown_method`, `setup_class`, `teardown_class`, `setup`, `teardown`) or module (`setup_module`, `teardown_module`, `setup_function`, `teardown_function`).
3. Module source contains none of `django.core.mail`, `override_settings`, `modify_settings`, `django.contrib.sites`, `set_urlconf`, or `django.template`.
4. No markers other than `parametrize`; function is not a coroutine (`async def`).
5. Every test parameter in `fixtureinfo.argnames` is supplied directly by `callspec.params`.
6. **`indirect=True` safety**: `name2fixturedefs` is inspected *before* accepting any name in `callspec.params`. Only pytest's `get_direct_param_fixture_func` wrapper passes; any parameter backed by a real fixture definition (`@pytest.mark.parametrize(..., indirect=True)`) is rejected and routed through full `runtestprotocol`.
7. Every active autouse fixture falls into one of three verified buckets:
   - Built-in `_pytest.*` / `pytest_django.*` no-op guards (module origin is verified at runtime so user-defined fixtures can never collide by name).
   - Opt-in project fixture names explicitly listed in `.pyrn.toml` `pure_autouse_guards` (only when autouse, never when requested as a parameter).
   - Zero-argument, non-generator, non-async, function-scoped autouse cleanup functions, which `_run_direct_fast_path` invokes explicitly before calling the test.

Any item that fails any check runs through `_pytest.runner.runtestprotocol` with exact pytest semantics.

## Built-in `--cov-sysmon` coverage

Passing `--cov-sysmon` enables CPython 3.12+ `COVERAGE_CORE=sysmon` coverage without `pytest-cov`:

- Each worker writes to its own isolated `.coverage.<worker_id>` shard in an ephemeral temp directory outside the repository.
- Warm daemons always export `COVERAGE_CORE=sysmon` at boot so per-run `--cov-sysmon` requests require zero worker restarts.
- Because the worker's PEP 669 dynamic tracer allocates tool IDs from `(5, 4, 3, 2)` and avoids `COVERAGE_ID (1)`, `--cov-sysmon` and `--trace` / `--cache` compose simultaneously without conflict.
- After execution completes, Rust invokes `<python> -m coverage combine --data-file <target>` (default `<runtime_dir>/coverage/.coverage`, overridable via `--cov-out`) and removes the temporary shard directory.

## Watchdogs, crash isolation, and multi-fault flake reruns

- **Hard watchdogs**: Workers emit `WorkerEvent::TestStart` before every test. The Rust supervisor enforces `--test-timeout` (default `60` s, clamped `>= 1` s) while a test is active, and `max(45, 2 * test_timeout)` seconds during unit collection.
- **Process-tree termination & requeue**: On timeout, segfault, `os._exit(...)`, or unexpected EOF:
  1. The supervisor terminates the entire worker process tree (`taskkill /F /T /PID` on Windows; `child.kill()` + `wait()` on POSIX, zero `unsafe`).
  2. Only the active test is recorded as `timed_out` or `crashed` (with the last 2 KiB of captured `stderr`).
  3. All remaining unexecuted `collected_nodeids` from that unit are pushed to the front of the queue as a `recover-NNNN` unit with `explicit_nodeids`.
  4. The slot respawns a clean worker up to `MAX_RESTARTS_PER_SLOT = 16`, guaranteeing termination instead of infinite crash loops.
- **Crash-safe flake reruns (`--reruns N`)**: After the main pool finishes, non-`timed_out` failing tests (`failed`, `error`, `crashed`) are rerun up to `N` times per file. Any failing test that passes on rerun is reclassified as `TestOutcome::Flaky`. If a worker crashes or times out *during* a flake rerun, the supervisor attributes the fault, respawns a fresh worker (within `MAX_RESTARTS_PER_SLOT`), and finishes rerunning the remaining failing tests in that file.

## Related guides

- [Resource budgeting](resource-budgeting.md) — how `-w auto` sizes the worker pool.
- [Receipts and telemetry](receipts-and-telemetry.md) — `RunReport`, `CompactReceipt`, `StreamEvent`, and `--metrics-kv`.
- [Benchmarks](benchmarks.md) — measured cold, warm-daemon, and `--cov-sysmon` performance.
- [Troubleshooting](troubleshooting.md) — handling custom fixtures, hangs, and daemon resets.
