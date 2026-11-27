# Security model

`pyrn` is designed with narrow local IPC boundaries, strict cache isolation, fail-closed test
selection defaults, and memory-safe Rust across the entire workspace.

## Pure safe Rust (`#![forbid(unsafe_code)]`)

Every crate in the workspace (`pyrn-core`, `pyrn-runner`, and `pyrn-cli`) enforces
`unsafe_code = "forbid"` at the workspace lint level and via crate-root `#![forbid(unsafe_code)]`.

- The default `pyrn-core` library build (`default-features = false`) has zero C/SQLite FFI, zero
  subprocess spawning, and zero filesystem walking.
- The Python distribution (`pyrn` wheel) is a zero-dependency launcher that invokes the bundled
  standalone Rust executable via `os.execve` (POSIX) or `subprocess.call` (Windows)—it contains no
  PyO3 bindings or custom CPython C extensions.

## Authenticated loopback worker and daemon IPC

Both the cold worker supervisor and the persistent warm daemon communicate with Python workers and
CLI clients strictly over local loopback TCP (`127.0.0.1:0`, ephemeral port):

- Every daemon RPC listener and every worker pool listener generates a fresh, independent **128-bit
  cryptographically secure random hex session token** via `getrandom`.
- Workers must present the exact worker session token on handshake; daemon RPC requests (`Status`,
  `Run`, `Stop`) must present the exact daemon `auth_token` stored in the role-scoped
  `daemon.json` state file. Unauthorized requests are rejected with an error response.
- No listeners bind to `0.0.0.0` or external network interfaces.

## Role-segregated caching (`SlotRole::{Agent, Verify}`)

`pyrn` separates interactive development and AI/PR agent runs (`--role agent`) from clean
verification gates (`--role verify`) across both storage layout and database keys:

- Separate on-disk directory trees (`<cache_base>/<role>/<project_fp>/...`) for `cas_cache.sqlite3`,
  `daemon.json`, timing history, and SQLite DB templates.
- SQLite WAL cache rows use the composite primary key
  `(role, project_fp, env_fingerprint, cache_key)`, where `cache_key` is a domain-separated 64-hex
  BLAKE3 closure digest (`pyrn.test-key.v1`).
- Cache lookups (`lookup_passing_hits`) strictly serve only `outcome == "passed"` non-tainted rows.
  Failed, errored, skipped, flaky, or tainted tests are never served as cache hits, and `agent`
  entries never satisfy `verify` queries.

## Authoritative `base_tree` policy in `pyrn-core`

When embedding `pyrn-core` via `plan_python_tests(Some(base_tree), candidate_tree, ...)`, project
configuration (`.pyrn.toml` and `pyproject.toml`) is loaded strictly from `base_tree` whenever
`base_tree` is non-empty (falling back to `candidate_tree` only on an empty-repo seed).

This prevents an untrusted candidate branch or pull request from editing `.pyrn.toml` to disable
`symbol_level_tia`, switch `unknown_path_policy` from `full` to `mapped_only`, or remove
`extra_full_triggers` in order to suppress required tests.

## Worker environment sanitization and isolation

Before spawning each Python worker, `WorkerSandbox` constructs a clean runtime boundary:

- Strips ambient pytest, xdist, and coverage environment variables (`PYTEST_CURRENT_TEST`,
  `PYTEST_XDIST_*`, `PYTEST_ADDOPTS`, `COVERAGE_FILE`, `COVERAGE_CORE`, `COV_CORE_*`).
- Assigns a dedicated RAII-cleaned ephemeral temp directory (`TMPDIR`, `TEMP`, `TMP`) that is
  removed when the worker exits.
- Assigns a deterministic disjoint block of 100 loopback ports (`PYRN_PORT_START`..`PYRN_PORT_END`
  and `DJANGO_LIVE_TEST_SERVER_ADDRESS`) to prevent cross-worker socket collisions.

## Bounded machine outputs and fail-closed defaults

- `CompactReceipt` (`pyrn.receipt.v1`) is strictly bounded to **`<= 2,048` bytes**
  (`MAX_RECEIPT_BYTES`), and inline `CrashCapsule` failure witnesses are bounded to **`<= 128`
  bytes** (`MAX_CRASH_CAPSULE_BYTES`) with UTF-8 codepoint-safe truncation.
- `UnknownPathPolicy::Full` is the default: modifying an unmapped file or global config upgrades
  impact selection to `FullVerified` rather than silently skipping tests.
- Git helpers (`pyrn plan --base`, `pyrn replay-git`) invoke `git` strictly read-only without
  fetching or mutating the working tree.

## Execution trust boundary

Executing `pyrn run` or `pyrn daemon start` runs the target repository's Python source code,
`conftest.py` plugins, and test suite under the invoking OS user. Per-worker sandboxes prevent
accidental cross-test interference—they are **not** a security sandbox against malicious code.
Always run untrusted pull requests inside isolated, ephemeral CI containers or VMs with
least-privilege permissions.

## Reporting a vulnerability

Do not open a public GitHub issue for a suspected security vulnerability. Report issues privately
via [GitHub private vulnerability reporting](https://github.com/dariuszpanas/pyrn/security/advisories/new)
and review [SECURITY.md](https://github.com/dariuszpanas/pyrn/blob/main/SECURITY.md) before
submitting. Include the affected version or commit, reproduction steps, and expected fail-closed
behavior.
