# Embedding `pyrn-core`

`pyrn-core` is designed from the ground up to embed inside Rust CI services, merge gates, code-review orchestrators, and custom test planners. With default features (`default-features = false` or `default = []`), it compiles with `#![forbid(unsafe_code)]` (Rust edition `2024`) as **100% pure in-memory Rust**:

- **Zero C or SQLite FFI** (no `rusqlite` or `libsqlite3-sys`)
- **Zero filesystem walking** (no `ignore` or directory I/O)
- **Zero subprocesses** (no `git` or Python execution)

All AST parsing, symbol-level diffing, dependency graph construction, BLAKE3 cache-key generation, and bounded receipt creation run strictly over in-memory byte slices exposed via the `TreeSource` trait.

## Cargo feature matrix

| Feature | Cargo definition | Extra dependencies | Unlocked APIs |
| --- | --- | --- | --- |
| `default` | `[]` | Pure Rust only (`anyhow`, `blake3`, `rayon`, `rustpython-parser`, `serde`, `serde_json`, `toml`) | `TreeSource`, `InMemoryTree`, `PyrnConfig::from_tree`, `parse_python_file`, `diff_python_symbols`, `diff_trees_with_symbols`, `DependencyGraph`, `plan_python_tests`, `DynamicTraceIndex::from_json_str`, `TestCacheKeyRecord`, `CompactReceipt`, `CrashCapsule`, `RunMetrics`, `StreamEvent`, `DOMAIN_*`, `blake3_domain_hex` |
| `fs` | `["dep:ignore"]` | `ignore` | `InMemoryTree::load_from_fs`, `FsTreeSource` alias, `PyrnConfig::load_from_dir`, `DynamicTraceIndex::load_json` |
| `cache` | `["dep:rusqlite"]` | `rusqlite` (bundled SQLite) | `cache` module: `CasTestCache`, `CachedTestEntry`, `CacheStats` |
| `process` | `[]` | None (`std::process::Command`) | `git_replay` module: `git_changed_specs_against_base`, `replay_git_history`, `git_common_dir`, `CommitReplayRow`, `ReplaySummary` |
| `full` | `["fs", "cache", "process"]` | All of the above | Full host feature set used by `pyrn-cli` |

Add the pure in-memory core to your `Cargo.toml`:

```toml
[dependencies]
pyrn-core = { version = "0.2.1", default-features = false }
```

Verify a zero-FFI build at any time with:

```sh
cargo check -p pyrn-core --no-default-features
```

## Implementing `TreeSource`

`TreeSource` is the single read-only abstraction connecting your storage backend (Git object database, virtual overlay, S3/CAS snapshot, or in-memory Merkle tree) to `pyrn-core`:

```rust
pub trait TreeSource: Sync {
    /// Sorted repository-relative POSIX paths (e.g. `src/myapp/admin.py`).
    fn paths(&self) -> &[String];

    /// Return raw bytes for a repository-relative POSIX path, if present.
    fn get(&self, rel_path: &str) -> Option<&[u8]>;

    /// Whether a repository-relative POSIX path exists in this tree.
    fn contains(&self, rel_path: &str) -> bool {
        self.get(rel_path).is_some()
    }
}
```

Implementation invariants:

- **Thread safety (`Sync`)**: `DependencyGraph::build` hashes and parses files in parallel across Rayon worker threads.
- **Normalized POSIX paths**: every entry returned by `paths()` and accepted by `get()` must use forward slashes (`/`) with no leading `./` (call `pyrn_core::normalize_rel_path` when indexing).
- **Deterministic ordering**: keep `paths()` sorted so fingerprints and outputs are deterministic.

### Custom `TreeSource` adapter example

```rust
use pyrn_core::{TreeSource, normalize_rel_path};
use std::collections::BTreeMap;

pub struct SnapshotTree {
    paths: Vec<String>,
    blobs: BTreeMap<String, Vec<u8>>,
}

impl SnapshotTree {
    pub fn new(entries: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
        let mut blobs = BTreeMap::new();
        for (path, bytes) in entries {
            blobs.insert(normalize_rel_path(&path), bytes);
        }
        let paths = blobs.keys().cloned().collect();
        Self { paths, blobs }
    }
}

impl TreeSource for SnapshotTree {
    fn paths(&self) -> &[String] {
        &self.paths
    }

    fn get(&self, rel_path: &str) -> Option<&[u8]> {
        self.blobs.get(rel_path).map(Vec::as_slice)
    }
}
```

## Using `InMemoryTree`

For quick integration, testing, or small-to-medium snapshots, `pyrn-core` ships `InMemoryTree` (which implements `TreeSource` out of the box):

| Method | Signature | Behavior |
| --- | --- | --- |
| `InMemoryTree::default()` | `() -> Self` | Empty tree. |
| `InMemoryTree::from_map` | `(BTreeMap<String, Vec<u8>>) -> Self` | Normalizes keys to POSIX paths and builds sorted path index. |
| `insert` | `(&mut self, impl Into<String>, Vec<u8>)` | Inserts or replaces a file blob and keeps `paths` sorted. |
| `remove` | `(&mut self, &str) -> Option<Vec<u8>>` | Removes a file blob and updates `paths`. |
| `len` / `is_empty` | `(&self) -> usize` / `bool` | Number of indexed files. |
| `InMemoryTree::load_from_fs` | `(&Path) -> anyhow::Result<Self>` (`fs` feature) | Parallel `.gitignore`-aware repository loader skipping `.git`, `.venv`, `node_modules`, `target`, caches, and binary assets. |

## Authoritative `base` vs `candidate` trees

`plan_python_tests` is the primary high-level planning entry point:

```rust
pub fn plan_python_tests(
    base_tree: Option<&dyn TreeSource>,
    candidate_tree: &dyn TreeSource,
    dynamic: Option<&DynamicTraceIndex>,
    opts: &PlanOptions,
) -> TestPlan
```

Key soundness invariants:

1. **Policy authority**: `plan_python_tests` reads `PyrnConfig` (`pyproject.toml`, `.pyrn.toml`, `pyrn.toml`) from `base_tree` whenever `base_tree` is `Some` and non-empty (falling back to `candidate_tree` only when `base_tree` is `None` or empty, such as an initial empty-repo commit). A candidate patch cannot edit `.pyrn.toml` to disable `unknown_path_policy = "full"` or remove `extra_full_triggers` on its own PR.
2. **Zero-checkout symbol diffing**: When `opts.explicit_changed` is `None` and `base_tree` is `Some(bt)`, `plan_python_tests` calls `diff_trees_with_symbols(bt, candidate_tree, &auth_project)` to compute exact `ChangedSymbols::{None, Some, All}` diffs purely in memory.
3. **Fail-closed default**: When both `base_tree` and `opts.explicit_changed` are `None`, `plan_python_tests` conservatively selects the entire suite (`ConfidenceTier::FullVerified`).

### `PlanOptions` fields

| Field | Type | Default |
| --- | --- | --- |
| `tier` | `ConfidenceTier` | `ConfidenceTier::StrictImpacted` |
| `role` | `SlotRole` | `SlotRole::Agent` |
| `platform_tag` | `String` | `"<os>-<arch>"` |
| `python_version` | `String` | `"3.12"` |
| `django_settings_module` | `String` | `"default"` |
| `env_values` | `BTreeMap<String, String>` | Empty |
| `explicit_changed` | `Option<Vec<ChangedFileSpec>>` | `None` (diff `base_tree` vs `candidate_tree`) |

### `TestPlan` fields

| Field | Type | Description |
| --- | --- | --- |
| `env_fingerprint` | `String` | 64-hex `pyrn.env-fp.v1` BLAKE3 digest. |
| `project_fingerprint` | `String` | 64-hex `pyrn.project-fp.v1` BLAKE3 digest. |
| `impact` | `ImpactResult` | Effective tier, fallback reasons, `symbol_pruned_edges`, `reduction_pct`, and per-file `impacted` reasons. |
| `selected_files` | `Vec<String>` | Sorted repo-relative test file paths to execute. |
| `selected_nodeids` | `Vec<String>` | Static test nodeids belonging to `selected_files`. |
| `cache_keys` | `Vec<TestCacheKeyRecord>` | 64-hex `pyrn.test-key.v1` cache keys and `CacheSafety` per discovered test node. |
| `metrics` | `RunMetrics` | Ready for `.to_kv_line(prefix)` / `.to_kv_lines(prefix)`. |
| `receipt` | `CompactReceipt` | Bounded `pyrn.receipt.v1` receipt (`receipt.to_bounded_json().len() <= MAX_RECEIPT_BYTES`). |
| `parse_and_hash_ms` | `f64` | Parallel Rust AST parse + BLAKE3 time (ms). |
| `graph_build_ms` | `f64` | Dependency graph build time (ms). |

## Complete copy-paste Rust example

Every imported symbol below is re-exported directly from the `pyrn_core` crate root and compiles cleanly with `default-features = false`:

```rust
use pyrn_core::{
    ConfidenceTier, DEFAULT_METRICS_PREFIX, DynamicTraceIndex, ExecVerdict, InMemoryTree,
    MAX_RECEIPT_BYTES, PlanOptions, SlotRole, plan_python_tests,
};
use std::collections::BTreeMap;

fn main() {
    // 1. Construct the authoritative base repository snapshot (e.g. target branch HEAD)
    let mut base = InMemoryTree::default();
    base.insert(
        ".pyrn.toml",
        br#"
testpaths = ["tests"]
[tia]
symbol_level_tia = true
unknown_path_policy = "full"
"#
        .to_vec(),
    );
    base.insert(
        "src/math_utils.py",
        b"def add(a, b):\n    return a + b\n\ndef mul(a, b):\n    return a * b\n".to_vec(),
    );
    base.insert(
        "tests/test_add.py",
        b"from src.math_utils import add\n\ndef test_add():\n    assert add(2, 3) == 5\n".to_vec(),
    );
    base.insert(
        "tests/test_mul.py",
        b"from src.math_utils import mul\n\ndef test_mul():\n    assert mul(2, 3) == 6\n".to_vec(),
    );

    // 2. Construct the candidate snapshot with a symbol-only edit to `add`
    let mut candidate = base.clone();
    candidate.insert(
        "src/math_utils.py",
        b"def add(a, b):\n    return a + b + 0\n\ndef mul(a, b):\n    return a * b\n".to_vec(),
    );

    // Optional: parse dynamic traces purely in memory (no filesystem needed)
    let dynamic: Option<DynamicTraceIndex> = None;

    // 3. Plan impacted tests and BLAKE3 cache keys with zero checkout & zero subprocesses
    let opts = PlanOptions {
        tier: ConfidenceTier::StrictImpacted,
        role: SlotRole::Verify,
        platform_tag: "linux-x86_64".to_owned(),
        python_version: "3.12".to_owned(),
        django_settings_module: "default".to_owned(),
        env_values: BTreeMap::new(),
        explicit_changed: None,
    };

    let plan = plan_python_tests(Some(&base), &candidate, dynamic.as_ref(), &opts);

    // Symbol-level TIA prunes the unaffected `tests/test_mul.py` edge
    assert_eq!(plan.selected_files, vec!["tests/test_add.py".to_owned()]);
    assert_eq!(plan.impact.symbol_pruned_edges, 1);
    assert_eq!(plan.receipt.verdict, ExecVerdict::Pass);

    let receipt_bytes = plan.receipt.to_bounded_json();
    assert!(receipt_bytes.len() <= MAX_RECEIPT_BYTES);

    println!("Selected files: {:?}", plan.selected_files);
    println!("Cache keys computed: {}", plan.cache_keys.len());
    println!("{}", plan.metrics.to_kv_line(DEFAULT_METRICS_PREFIX));
}
```

## Exported BLAKE3 domain constants

Embedders implementing custom cache stores or verification ledgers can reuse `pyrn_core::blake3_domain_hex(domain, data)` with the exact domain separators used internally:

| Constant | Value | Used for |
| --- | --- | --- |
| `DOMAIN_RAW_BLOB` | `"pyrn.blob.raw.v1"` | Raw file content bytes |
| `DOMAIN_AST_BLOB` | `"pyrn.blob.ast.v1"` | Canonical comment/whitespace-invariant module AST |
| `DOMAIN_AST_SYMBOL` | `"pyrn.symbol.ast.v1"` | Canonical top-level symbol / test function AST |
| `DOMAIN_AST_PREAMBLE` | `"pyrn.preamble.ast.v1"` | Executable module-level preamble statements |
| `DOMAIN_TEST_KEY` | `"pyrn.test-key.v1"` | Per-test content-addressed cache key |
| `DOMAIN_ENV_FP` | `"pyrn.env-fp.v1"` | Environment fingerprint |
| `DOMAIN_PROJECT_FP` | `"pyrn.project-fp.v1"` | Project identity fingerprint |

## Related guides

- [Test Impact Analysis](tia.md) — symbol pruning rules, Django semantic edges, and soundness limits.
- [Content-addressed caching](caching.md) — `CasTestCache` schema and `SlotRole` segregation when enabling the `cache` feature.
- [Receipts and telemetry](receipts-and-telemetry.md) — `CompactReceipt`, `CrashCapsule`, and `RunMetrics` contracts.
- [Security](security.md) — `#![forbid(unsafe_code)]`, zero-checkout threat model, and policy authority.
