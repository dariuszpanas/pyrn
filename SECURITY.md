# Security Policy

Do not open a public GitHub issue for a suspected security vulnerability. Report security issues
privately through [GitHub private vulnerability reporting](https://github.com/dariuszpanas/pyrn/security/advisories/new)
with the affected version or commit, reproduction steps, and expected fail-closed behavior.

For full architectural details on `pyrn`'s isolation, authentication, and trust boundaries, see the
[Security model documentation](https://dariuszpanas.github.io/pyrn/security.html)
([docs/security.md](docs/security.md)).

## Security Properties

- **Memory safety**: Every Rust crate in the workspace enforces `#![forbid(unsafe_code)]` via
  workspace lints (`[workspace.lints.rust] unsafe_code = "forbid"`). The Python wheel launcher
  bundles the standalone binary directly without PyO3 or custom C extensions.
- **Authenticated loopback IPC**: Worker pools and warm daemons bind exclusively to `127.0.0.1:0`
  and authenticate every worker handshake and daemon RPC request with a fresh 128-bit CSPRNG hex
  token generated via `getrandom`.
- **Role-segregated caching**: `SlotRole::Agent` and `SlotRole::Verify` maintain separate on-disk
  cache directories, daemon state files, and SQLite primary-key partitions. Failed, skipped,
  errored, flaky, or tainted tests are never served as cache hits.
- **Authoritative base-tree policy**: Embedded callers using `plan_python_tests(Some(base_tree),
  candidate_tree, ...)` load `.pyrn.toml` and `pyproject.toml` from `base_tree` (falling back to
  `candidate_tree` only when `base_tree` is absent or empty), preventing untrusted candidate edits
  from weakening test selection policy.
- **Bounded machine outputs**: `CompactReceipt` JSON is strictly bounded to `<= 2,048 B` and
  `CrashCapsule` failure witnesses to `<= 128 B`.

## Execution Boundary

Running `pyrn run` or `pyrn daemon start` executes your repository's Python code, `conftest.py`
hooks, and test functions under the invoking user's OS account. Worker sandboxes isolate temp
directories, environment variables, and loopback ports to prevent cross-test interference, not to
contain malicious code. Run untrusted pull requests inside isolated ephemeral CI containers or VMs.
