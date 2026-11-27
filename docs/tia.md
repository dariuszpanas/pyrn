# Test Impact Analysis (TIA)

`pyrn`'s Test Impact Analysis engine (`crates/pyrn-core`) analyzes a Python repository purely in memory in Rust—with zero Python subprocesses, zero filesystem walking in the default crate build, and zero repository mutations—to determine which test files can be affected by a change.

## Analysis pipeline

`DependencyGraph::build` executes four deterministic phases over any [`TreeSource`](embedding.md#implementing-treesource):

1. **Parallel parsing and dual BLAKE3 hashing (`rayon` + `rustpython-parser`)**:
   - Every indexed file receives a domain-separated raw BLAKE3 digest (`pyrn.blob.raw.v1`).
   - Every `.py` file is parsed with Python 3.12+ grammar into a canonical AST digest (`pyrn.blob.ast.v1`, invariant to comments, blank lines, and formatting), a `module_preamble_hash` (`pyrn.preamble.ast.v1`), and a top-level symbol table (`pyrn.symbol.ast.v1`).
   - Files that fail UTF-8 decoding or AST parsing degrade cleanly via `fallback_parsed`: `parse_fallback = true`, `ast_hash = raw_hash`, `uses_source_introspection = true`, line-based import/test scraping, and `parse_fallback` taint (never served from cache).
2. **Source root and module indexing**:
   - Combines `PyrnConfig.source_roots` (from `[tool.hatch.build.targets.wheel].packages`), auto-detected `src/` (when any `src/*/__init__.py` exists), and the repository root `""` to build bidirectional `module_to_path` / `path_to_module` tables.
3. **Typed forward dependency graph**:
   - Extracts static and local imports, package `__init__.py` ancestors, `pytest_plugins`, string-based dotted module refs (`patch`, `import_module`, `import_string`, `setattr`, `ROOT_URLCONF`, `BACKEND`, `callable_path`, `DJANGO_SETTINGS_MODULE`), `conftest.py` hierarchy and fixture bindings, Django semantic groups, `Path / "literal"` and string asset references, and optional dynamic trace edges.
4. **Symbol-aware reverse BFS (`compute_impact_with_symbols`)**:
   - Walks `reverse_deps` from each semantically changed file while tracking both the narrowed set of affected exported symbols and synthetic-edge restrictions, pruning edges provably unaffected by the change.

### Dependency edge kinds

Every edge in `forward_deps` carries one or more typed `EdgeKind` tags and a `SymbolFilter`:

| `EdgeKind` | Source | Default `SymbolFilter` |
| --- | --- | --- |
| `static_import` | Module-level `import` / `from ... import ...` | `Named(syms)` or `All` |
| `lazy_import` | Function/method/class-local `import` / `from ... import ...` (tracks `enclosing_symbol`) | `Named(syms)` or `All` |
| `package_init` | Ancestor `pkg/__init__.py` executed when importing a submodule | `PreambleOnly` |
| `string_module_ref` | Dotted module strings in `patch(...)`, `import_module(...)`, `import_string(...)`, `setattr(...)`, config dicts/kwargs, or dotted string literals | `All` |
| `conftest_file` | Test file's ancestor `conftest.py` files | `All` |
| `conftest_top_import` | Top-level imports inside `conftest.py` | `Named(syms)` or `All` |
| `conftest_plugin` | Module strings declared in `pytest_plugins` | `All` |
| `fixture_dep` | Test/fixture parameter matched to a `conftest.py` `@pytest.fixture` and its local imports | `All` |
| `django_db_models` | DB test -> `*/models.py`, `*/models/*.py`, `*/apps.py` | `All` |
| `django_db_migrations` | DB test -> `*/migrations/*.py` | `All` |
| `django_settings` | DB / URL / `override_settings` / `django.conf` test -> `*/conf/*.py` (and `tests/postgres_settings.py` for `@pytest.mark.postgresql`) | `All` |
| `django_admin` | Admin test (`admin_client`, `admin_user`, `django.contrib.admin`) -> `*/admin.py`, `*/admin/*.py` | `All` |
| `django_urls_templates` | URL/template/client test (`client`, `rf`, `reverse`, `django.urls`, `django.template`, `django.test`) -> views/URLs/templates/static assets | `All` |
| `django_management_command` | `call_command("<cmd>")` -> `*/management/commands/<cmd>.py` | `All` |
| `repo_asset_literal` | Literal repo-relative file path or `Path / "a" / "b"` chain | `All` |
| `repo_asset_dir_prefix` | Watched non-code-root directory literal (also registers `dir_watchers` for newly added files) | `All` |
| `dynamic_trace_py` | Runtime-executed `.py` file from PEP 669 `sys.monitoring` trace (`Named(top_syms)` when `--trace-symbols` captured qualnames, else `All`) | `Named(syms)` or `All` |
| `dynamic_trace_asset` | Runtime-opened asset (`open`) or listed directory (`os.listdir`/`os.scandir`) from PEP 578 audit hook | `All` |

## Symbol-level diffs and preamble fail-closed guards

Hub modules (shared helpers, settings utilities, re-export barrels) cause massive false-positive fanout under file-level TIA. Under `StrictImpacted` with `symbol_level_tia = true` (the default), `pyrn-core` partitions every parsed module into:

- **Top-level symbols (`symbols`)**, classified by `SymbolKind`:
  - `Function` (`def`, `async def`)
  - `Class` (`class`)
  - `Constant` (single-target `Assign` / `AnnAssign` whose value is a pure side-effect-free expression: literals, containers, `frozenset`, `set`, `tuple`, `dict`, `list`, `re.compile`, `PurePosixPath`, unary/binary ops, names, attributes)
  - `StdlibImport` (top-level `import` / `from` strictly within a curated allowlist of side-effect-free stdlib roots such as `os`, `pathlib`, `re`, `json`, `typing`, `secrets`, `datetime`)
  - `ImportedName` (non-stdlib `from mod import sym`: hashed inside `module_preamble_hash` AND recorded as a symbol so symbol-only edits to `sym` propagate strictly to functions or re-exporters that reference `sym`)
- **Executable module preamble (`module_preamble_hash` + `preamble_referenced_names`)**:
  - Every other top-level statement (module-level side-effect calls, decorator registrations, dynamic assignments, wildcard imports, non-stdlib imports).
  - Non-import-time `if` guards (`if __name__ == "__main__":`, `if TYPE_CHECKING:`, `if typing.TYPE_CHECKING:` with empty `else`), module docstrings at statement index `0`, and `__all__ = [...]` are still hashed into `module_preamble_hash` (direct edits fail closed to `All`), but their identifiers are excluded from `preamble_referenced_names` so editing a function referenced only inside `if __name__ == "__main__":` does not falsely trigger import-time fanout.

### `diff_python_symbols(before, after)` rules

| Condition | Result (`ChangedSymbols`) |
| --- | --- |
| Both parsed cleanly and `before.ast_hash == after.ast_hash` | `None` (comment, whitespace, or formatting-only edit) |
| Either side has `parse_fallback` and `raw_hash` differs | `All` |
| `before.module_preamble_hash != after.module_preamble_hash` | `All` |
| Any top-level symbol in `before.symbols` was deleted or renamed | `All` (guards against `ImportError` in re-exporters) |
| Only a subset of top-level symbol hashes changed or were added | `Some { names }` |
| File was added, deleted, or non-Python | `All` |

### Intra-file and multi-hop symbol propagation

When a file changes with `ChangedSymbols::Some { names }`:

1. **Intra-file reverse closure (`affected_exports`)**: computes all top-level symbols in that file that directly or transitively reference any symbol in `names`. If any affected symbol appears in `preamble_referenced_names`, `affected_exports` returns `None` (fails closed to whole-module `All` because import-time behavior may have changed for every importer).
2. **Edge pruning during reverse BFS**:
   - `SymbolFilter::Named(imported)` where every imported name exists in the target's symbol table and `imported` is disjoint from `affected_exports`: **pruned** (increments `symbol_pruned_edges`).
   - `SymbolFilter::PreambleOnly` (ancestor `__init__.py` edges) at `depth > 0`: **pruned**, because the `__init__.py` itself was not edited and its preamble does not reference any affected symbol.
   - When an overlap exists across a `lazy_import` inside `caller_symbols`, propagation narrows to `dep.affected_exports(caller_symbols)`. Across a top-level `from curr import x` (`SymbolKind::ImportedName`), propagation narrows to `dep.affected_exports(&overlap)`—automatically failing closed to `All` if `dep`'s preamble executes the imported symbol.

### Comment-only edits and source introspection

Under `StrictImpacted`, files with `ChangedSymbols::None` are classified as `comment_or_format_only_files` and skipped for ordinary tests—**except** for tests where `uses_source_introspection` is true (directly or transitively). The parser sets `uses_source_introspection = true` whenever a file calls `inspect.getsource`, `inspect.getsourcelines`, `ast.parse`, `tokenize.generate_tokens`, accesses `.__doc__`, or hits `parse_fallback`. Those tests still re-run on comment/docstring/formatting edits and automatically upgrade to `raw_hash` cache keys.

## `conftest.py` scoping policies

Directly modifying any `conftest.py` selects every test in its directory subtree; additionally, root `conftest.py` and `tests/conftest.py` are built-in global triggers (`FullVerified`).

For indirect dependencies *through* `conftest.py`, `conftest_policy` controls how function-local imports inside fixtures propagate:

| Policy | Behavior |
| --- | --- |
| `smart_fixture_scoped` (default under `strict`) | Top-level imports (`conftest_top_import`) and `pytest_plugins` (`conftest_plugin`) apply to all subtree tests. Local imports inside non-autouse fixtures apply only to tests requesting that fixture (`fixture_dep`). Local imports inside DB-conditional autouse fixtures (detected by a `django_db`/`transactional_db` guard with early `return`, or listed in `pure_autouse_guards`) and pure attribute-reset cache clearers (`try: import mod; mod._x = None except ...: pass`) do not fan out globally. |
| `strict_all_imports` (forced under `transitive` and `full`) | Every import anywhere in `conftest.py`—including local imports inside fixtures—fans out to every test in its subtree. |

## Django semantic rules

Django wires models, migrations, admin registrations, URLconfs, templates, settings, and management commands through app registries rather than direct Python imports. `pyrn-core` adds static semantic edges automatically without repository-specific configuration:

- **Models & apps (`django_db_models`)**: DB tests (`@pytest.mark.django_db`, `TestCase`/`TransactionTestCase` subclasses, `MigrationExecutor`, or `db`/`transactional_db`/`django_db_reset_sequences`/`django_db_serialized_rollback`/`admin_client`/`admin_user`/`live_server` fixtures) depend on all `*/models.py`, `*/models/*.py`, and `*/apps.py`.
- **Migrations (`django_db_migrations`)**: DB tests depend on all `*/migrations/*.py`, and modifying any `*/migrations/*.py` file selects all DB tests directly.
- **Settings (`django_settings`)**: DB tests, URL/client tests, and tests importing `django.conf` or calling `override_settings`/`modify_settings` depend on `*/conf/*.py` (and `@pytest.mark.postgresql` tests depend on `tests/postgres_settings.py` if present). **Symmetric traversal rule**: once a synthetic `django_settings` edge is crossed, traversal continues strictly along `static_import` and `package_init` chains, preventing unrelated `lazy_import` fanout through helper functions.
- **Admin (`django_admin`)**: Tests importing `django.contrib.admin` or using `admin_client`/`admin_user` depend on `*/admin.py`, `*/admin/*.py`, and admin templates.
- **URLs, views, and templates (`django_urls_templates`)**: Tests using `client`, `admin_client`, `rf`, `live_server`, `reverse`, `django.urls`, `django.template`, or `django.test` depend on `*/urls.py`, `*/views.py`, `*/api.py`, `*/api_boundary.py`, `*/route_policy.py`, `*/templates/*`, and `*/static/*`.
- **Management commands (`django_management_command`)**: `call_command("<cmd>")` creates an edge to `*/management/commands/<cmd>.py`.
- **Task backends**: Files importing `django.tasks` (or referencing `task_backends` on parse fallback) depend on `*/backends.py`.

## Zero-static inherited `unittest.TestCase` and dynamic collectors

Test modules that inherit test methods from a base class (`class TestSub(SharedMixin, unittest.TestCase): pass`) or generate tests dynamically (`pytest_generate_tests`, `load_tests`) contain zero static `def test_*` functions in their own AST.

`pyrn` handles them soundly at two layers:

1. **`pyrn-core` registration**: Any `.py` file whose basename matches `python_files` under a configured `testpaths` directory is registered in `DependencyGraph.tests_by_file` even when `parsed.tests` is empty, so `compute_impact_with_symbols` and `plan_python_tests` include it whenever its dependencies change or `FullVerified` triggers.
2. **`pyrn-runner` safety net**: During `pyrn run --tia`, any file returned by `discover_test_files` that is absent from `graph.tests_by_file` is still included whenever `effective_tier == FullVerified` or the file is reverse-reachable in `graph.reverse_deps` from any changed path.

## Dynamic tracing (PEP 669 + PEP 578 + `os.environ` proxy)

Static AST analysis can be enriched with runtime execution traces (`--trace`, `--trace-in`, `--trace-out`, or auto-collected during `--cache`). Both the embedded worker (`pyrn_worker.py`) and the standalone pytest plugin (`pytest_pyrn_trace.py`) use zero-monkeypatch CPython hooks:

1. **PEP 669 `sys.monitoring` (`PY_START` + `DISABLE`)**:
   - Registers a `PY_START` callback that records repo-relative `.py` files (and `rel_path::qualname` when `--trace-symbols` is set), then immediately returns `sys.monitoring.DISABLE` so each code object fires at most once per restart window (near-zero overhead).
   - Selects tool ID from `(OPTIMIZER_ID=5, 4, 3, 2)`—explicitly avoiding `COVERAGE_ID=1` so `--trace` and `--cov-sysmon` never collide.
   - Restarts events once per test file (`--trace-granularity file`, default) or once per test item (`--trace-granularity test`).
2. **PEP 578 `sys.addaudithook`**:
   - Observes `open` (read modes only, ignoring `.pyc`) -> `read_assets`.
   - Observes `os.listdir` and `os.scandir` -> `listed_dirs` (also subscribes `dir_watchers` for newly created files).
   - Observes `subprocess.Popen` -> records `dynamic:subprocess.Popen` taint plus any repo-relative file arguments as `read_assets`.
   - Observes `socket.connect` -> records `dynamic:socket.connect` taint.
3. **In-place `os.environ` read proxy**:
   - Wraps `type(os.environ).__getitem__` in place without replacing the `os.environ` instance, preserving `id(os.environ)`, `isinstance`, `monkeypatch.setenv`, and `mock.patch.dict`.
   - Captures `os.environ[k]`, `os.environ.get(k)`, `os.getenv(k)`, and `k in os.environ` (recording missing `KeyError` lookups as `None`) while ignoring ephemeral worker sandbox variables (`TMP`, `TEMP`, `TMPDIR`, `PYTEST_CURRENT_TEST`, `PYRN_WORKER_ID`, `PYRN_PORT_START`, `PYRN_PORT_END`, `DJANGO_LIVE_TEST_SERVER_ADDRESS`, `COVERAGE_*`, `COV_CORE_*`).

Standalone pytest plugin flags (`crates/pyrn-runner/python/pytest_pyrn_trace.py`):

| Option | Default | Effect |
| --- | --- | --- |
| `--pyrn-trace-out <PATH>` | unset | Enable tracing and write `DynamicTraceFile` JSON to `PATH`. |
| `--pyrn-granularity <file\|test>` | `file` | Restart `sys.monitoring` once per test file or once per test item. |
| `--pyrn-trace-symbols` | `false` | Record `rel_path::qualname` entries in `executed_symbols`. |
| `--pyrn-no-audit` | `false` | Disable PEP 578 `sys.addaudithook`. |

## Confidence tiers and fail-closed fallback

| Tier (`--tier`) | Serialized name | Hashing | Symbol pruning | `conftest.py` policy | Comment-only edits |
| --- | --- | --- | --- | --- | --- |
| `strict` (default) | `strict_impacted` | AST (auto-raw for source-introspecting / parse-fallback tests) | Enabled (if `symbol_level_tia = true`) | Configured (`smart_fixture_scoped` default) | Skipped (except source-introspecting tests) |
| `transitive` | `transitive_impacted` | Raw BLAKE3 everywhere | Disabled (file-level closure) | Forced `strict_all_imports` | Treated as semantic change |
| `full` | `full_verified` | Raw BLAKE3 everywhere | Disabled | Forced `strict_all_imports` | Selects entire test suite |

### Global triggers (automatic `full_verified`)

Modifying, adding, or deleting any of these paths unconditionally escalates `effective_tier` to `full_verified` (even under `unknown_path_policy = "mapped_only"`):

- `pyproject.toml`, `uv.lock`, `pytest.ini`, `setup.cfg`, `setup.py`, `.python-version`
- `.pyrn.toml`, `pyrn.toml`
- `conftest.py`, `tests/conftest.py`
- Any path listed in `extra_full_triggers`

### Unknown-path policy

If a semantically changed file matches no dependency edge, no directory watcher, and no migration rule:

- It is treated as non-impacting only if it matches a known documentation/metadata file (`.md`, `.txt`, `.rst`, `.png`, `.jpg`, `.svg`, `license`, `.gitignore`, `.gitattributes`, `.gitmessage`, `.editorconfig`) **or** it is a known Python module whose reverse edges were all legitimately pruned by symbol-level TIA (`file_symbol_pruned > 0`).
- Otherwise, under `unknown_path_policy = "full"` (the default), `pyrn` fails closed and escalates to `full_verified`. Under `unknown_path_policy = "mapped_only"`, unmapped paths do not trigger full fallback.

## Soundness limits and caveats

Static TIA is fast and fail-closed on unmapped files, but has inherent static-analysis boundaries you should account for when designing CI gates:

- **Computed dynamic imports**: `importlib.import_module(f"plugins.{name}")` or reflection built from non-literal strings cannot be resolved statically; collect dynamic traces (`--trace` / `--trace-in`) to record those runtime edges.
- **Unindexed third-party or binary changes**: edits to site-packages outside the repository without changing `uv.lock` or `pyproject.toml`, or changes to external services/databases, are invisible to repository tree diffing.
- **Custom import hooks or metaclass registries**: module registration executed only via side effects outside standard imports, `pytest_plugins`, or Django semantic conventions may require dynamic traces or `extra_full_triggers`.
- **Non-standard source-reading tests**: comment-only skipping auto-protects `inspect.getsource`, `inspect.getsourcelines`, `ast.parse`, `tokenize.generate_tokens`, and `.__doc__`; tests that read their own `.py` source via `Path(__file__).read_text()` should use `--tier transitive` or dynamic traces (which add a `dynamic_trace_asset` edge).
- **Recommended tiering**: use `--tier strict` for inner-loop development and PR iteration, and `--tier transitive` or `--tier full` for merge-queue and release verification. See [CI adoption](ci-adoption.md) and [Benchmarks](benchmarks.md).
