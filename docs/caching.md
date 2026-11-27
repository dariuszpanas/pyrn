# Content-addressed caching

`pyrn --cache` avoids re-executing unchanged passing tests using a multi-process SQLite WAL cache (`CasTestCache`) keyed by 64-hex domain-separated BLAKE3 closure digests (`pyrn.test-key.v1`). Cache keys contain **zero absolute paths**, so entries remain portable across commits, branches, and Git worktrees.

## Cache key construction (`pyrn.test-key.v1`)

`DependencyGraph::compute_all_cache_keys` computes a 64-hex BLAKE3 digest for each static test node by hashing these domain-separated components in deterministic order:

| Component | Domain / Format | What invalidates it |
| --- | --- | --- |
| 1. Key domain | `pyrn.test-key.v1\0` | Cache key schema version bump. |
| 2. Environment fingerprint | `pyrn.env-fp.v1` (64-hex BLAKE3 of `platform_tag`, `python_version`, `django_settings_module`, and raw hashes of `uv.lock`, `pyproject.toml`, `.python-version`, `pytest.ini`, `.pyrn.toml`, `pyrn.toml`, `tests/conftest.py`) | OS/arch change, Python version change, `DJANGO_SETTINGS_MODULE` change, lockfile/config/root conftest change. |
| 3. Confidence tier | `strict_impacted` \| `transitive_impacted` \| `full_verified` | Switching `--tier`. |
| 4. Test base nodeid | Repo-relative POSIX nodeid (without `[param]` suffix) | Renaming/moving test file, class, or function. |
| 5. Test AST hash | `pyrn.symbol.ast.v1` of the test `def`/`async def` | Editing the test body, signature, or decorators. |
| 6. Transitive closure digest | `pyrn.closure.v1\0` + mode (`raw` \| `sym` \| `ast`) + sorted `<dep_rel_path>=<digest>\n` | Editing any transitive Python module, `conftest.py`, fixture, Django schema/template, or static/dynamic asset in the test's closure. |
| 7. Allowlisted env vars | `ALLOW:<KEY>=<VALUE>\n` for every env var matching `env_allowlist_prefixes` (default `DJANGO_*`, `RAY_*`) | Adding, removing, or changing any allowlisted environment variable. |
| 8. Test-read env vars | `READ:<KEY>=SET:<VALUE>\n` or `READ:<KEY>=UNSET\n` for every key in `CacheSafety::EnvDependentCacheable` | Changing or unsetting an environment variable statically or dynamically read by the test. |

### Mathematical duality with symbol-level TIA

Under `StrictImpacted` with `symbol_level_tia = true`, the transitive closure digest uses `SymbolFilter` identically to `compute_impact_with_symbols`:

- `SymbolFilter::All` -> hashes full `parsed.ast_hash`.
- `SymbolFilter::Named(names)` -> hashes `parsed.symbol_scoped_digest(names)` (`pyrn.sym-slice.v1`: `module_preamble_hash` + intra-file symbol closure of `names`). Fails closed to full `ast_hash` if any requested name is unknown or `parse_fallback` is set.
- `SymbolFilter::PreambleOnly` -> hashes `init:<ast_hash>` for `__init__.py` or `pre:<module_preamble_hash>` for other modules.

The closure automatically upgrades to **raw file hashing** (`raw_hash`, mode `raw`) whenever:

- `--tier transitive` or `--tier full` is active,
- the test file has `uses_source_introspection = true` (`inspect.getsource`, `__doc__`, `ast.parse`, etc.), or
- any file has `parse_fallback = true`.

### Marker and keyword isolation (`cas_env_fp`)

When `pyrn run --cache` is invoked with `-m <markers>` or `-k <keyword>`, `pyrn-runner` derives a separate `cas_env_fp` (`pyrn.cas-env.v1` over `env_fingerprint`, `markers`, and `keyword`) before querying or seeding `CasTestCache`. Filtered runs and unfiltered runs therefore never collide in the cache table.

## SQLite WAL storage and concurrency

`CasTestCache` stores rows in SQLite with multi-process ACID settings:

- **Default path**: `<cache_dir>/<role>/<project_fp>/cas_cache.sqlite3` (where `<cache_dir>` defaults to `<OS temp>/pyrn-cache`, overridable via `--cache-dir` / `PYRN_CACHE_DIR`, and `<project_fp>` is the 16-hex BLAKE3 of the canonical repository root).
- **SQLite pragmas**: `journal_mode = WAL`, `synchronous = NORMAL`, `busy_timeout = 5000` (5 seconds).
- **Schema**:

```sql
CREATE TABLE IF NOT EXISTS test_cache (
    role TEXT NOT NULL,
    project_fp TEXT NOT NULL,
    env_fingerprint TEXT NOT NULL,
    cache_key TEXT NOT NULL,
    nodeid TEXT NOT NULL,
    outcome TEXT NOT NULL,
    duration_ms REAL NOT NULL,
    commit_sha TEXT NOT NULL,
    witness_128b TEXT NOT NULL,
    confidence_tier TEXT NOT NULL,
    created_at_epoch_s INTEGER NOT NULL,
    hit_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (role, project_fp, env_fingerprint, cache_key)
);
```

Additional indexes (`idx_test_cache_nodeid` on `(role, project_fp, nodeid)` and `idx_test_cache_created` on `(created_at_epoch_s)`) keep lookups and age-based pruning fast. Inside each SQLite row, `project_fp` is the 64-hex `pyrn.project-fp.v1` digest of `(source_roots, pyproject.toml raw hash)`—also free of absolute paths.

## `SlotRole` isolation (`agent` vs `verify`)

`SlotRole` segregates interactive development runs (`agent`, default) from clean CI/verification runs (`verify`):

1. **Filesystem segregation**: `CacheLayout::prepare` places `agent` and `verify` under separate subdirectories (`<cache_dir>/agent/...` vs `<cache_dir>/verify/...`), giving each role its own `cas_cache.sqlite3`, `daemon.json`, `timings.json`, and `dynamic_traces.json`.
2. **Primary-key segregation**: `role` is the leading column of `test_cache`'s `PRIMARY KEY (role, project_fp, env_fingerprint, cache_key)`, so even if an embedder shares one SQLite database across roles, `agent` entries can never be returned to a `verify` lookup.

## `CacheSafety` classification

Every discovered test node is classified into one of three `CacheSafety` states:

| Classification | Condition | Caching behavior |
| --- | --- | --- |
| `deterministic_cacheable` | No static or dynamic taint; no test-specific `os.environ` reads beyond allowlisted prefixes. | Eligible for CAS lookup and seeding. |
| `env_dependent_cacheable` | No static or dynamic taint; reads specific `env_keys` observed statically (`os.getenv`, `os.environ[...]`) or dynamically (`os.environ` proxy). | Eligible for CAS lookup and seeding; all `env_keys` (`SET:<val>` or `UNSET`) are hashed into `cache_key`. |
| `tainted_always_run` | One or more static or dynamic taint reasons present. | **Never cached and never served from cache.** Always executes when selected. |

### Exact taint triggers

A test is marked `tainted_always_run` if **any** of the following apply:

- **Markers (`marker:<name>`)**: `real_ray`, `live_cluster`, `postgresql`, `compiled_graph_opt_in`.
- **Fixtures (`fixture:<name>`)**: `ray_cluster`, `live_ray_cluster`, `live_server`.
- **Static calls (`call:<callee>`)**: `subprocess.run`, `subprocess.Popen`, `subprocess.check_output`, `subprocess.check_call`, `ray.init`, `init_local_ray`.
- **Static network calls (`network:<callee>`)**: `socket.create_connection`, `urllib.request.urlopen`, `requests.get`, `requests.post`.
- **Parse fallback (`parse_fallback`)**: AST parsing failed for the test file.
- **Dynamic audit hooks**: `dynamic:subprocess.Popen`, `dynamic:socket.connect` captured by PEP 578 `sys.addaudithook`.

## Sound seeding rules

`pyrn` enforces strict fail-safe guarantees when querying and seeding the cache:

1. **Passing-only lookups**: `CasTestCache::lookup_passing_hits` filters strictly on `outcome = 'passed'`.
2. **Passing-only family seeding**: `execute_unified_run` aggregates executed results by `base_nodeid` (stripping pytest `[param]` suffixes). A test family is seeded **only** when:
   - at least one variant executed and passed (`passed_n > 0`),
   - **zero** variants had any non-`passed` outcome (`failed`, `skipped`, `xfailed`, `xpassed`, `error`, `timed_out`, `crashed`, `flaky`), and
   - neither static `CacheSafety` nor dynamic runtime traces recorded any taint for that `base_nodeid`.
   `CasTestCache::put_passing_batch` enforces `outcome == "passed"` as a second guard at the database layer.
3. **Automatic dynamic trace verification**: Whenever `--cache` executes uncached tests, `pyrn-runner` automatically enables PEP 669 + PEP 578 + `os.environ` tracing, merges the traces into `<project_dir>/dynamic_traces.json`, and **recomputes cache keys** with any newly discovered `env_vars_read` or `read_assets` using the exact same `GraphConfig` before writing to SQLite.

## Negative skip-list guarantee and zero-worker cache hits

Rather than passing a positive allowlist of static AST nodeids to pytest (which would silently skip inherited `unittest.TestCase` methods or dynamically parametrized items), `pyrn` passes a **negative skip-list** (`exclude_nodeids_by_file`) containing only verified CAS hits.

Workers match exclusions by exact nodeid or base nodeid prefix before `[`. Any test not in the skip-list runs normally.

A test file can bypass worker execution completely—allowing a **0-worker, sub-second run** (`mode = "cold+cas"` or `"warm-daemon+cas"`, `worker_limit_reason = "all_cached"`) when all selected files qualify—only if **all four** conditions hold:

1. `--cache` is enabled.
2. The file has at least one static AST test node and **100%** of its static AST test nodes hit CAS.
3. Static source inspection (`file_may_have_non_static_tests`) confirms the file contains no `pytest_generate_tests`, `pytest_pycollect_makeitem`, `def load_tests`, `TestSuite`, and no `class Test*(...)` inheriting from anything outside known pure base classes (`object`, `TestCase`, `SimpleTestCase`, `TransactionTestCase`, `LiveServerTestCase`, `unittest.TestCase`, `django.test.TestCase`, `django.test.SimpleTestCase`, `django.test.TransactionTestCase`).
4. Every dynamically observed base nodeid previously recorded for that file in `dynamic_traces.json` is present in the CAS hit set.

If a file contains inherited mixins or dynamic test generators, `pyrn` still collects the file, skips only verified static hits via the negative skip-list, and executes all inherited or generated tests.

## Managing the cache

```sh
# Inspect entry counts, hit counters, and DB size
pyrn cache status --repo . --role agent

# Remove entries older than 14 days across the cache DB
pyrn cache prune --repo . --role agent --max-age-days 14

# Clear every entry for the verify role
pyrn cache prune --repo . --role verify --max-age-days 0 --only-role
```

See [`pyrn cache`](commands.md#pyrn-cache) for exact flags and JSON output schemas, and [CI adoption](ci-adoption.md) for cross-job cache sharing patterns.
