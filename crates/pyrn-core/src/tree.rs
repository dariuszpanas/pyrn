//! Pure-Rust abstract tree/blob source for zero-checkout in-memory TIA evaluation.
//!
//! All core AST analysis, symbol-level dependency graph construction, and BLAKE3
//! closure hashing operate strictly over `TreeSource` (`rel_path -> &[u8]`) with no
//! direct filesystem coupling.

use std::collections::BTreeMap;

/// Pure read-only view of a repository snapshot (filesystem, git tree, or in-memory Merkle tree).
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

/// In-memory repository snapshot suitable for embedded adapters, CLI runs, and tests.
#[derive(Debug, Clone, Default)]
pub struct InMemoryTree {
    paths: Vec<String>,
    blobs: BTreeMap<String, Vec<u8>>,
}

impl InMemoryTree {
    #[must_use]
    pub fn from_map(blobs: BTreeMap<String, Vec<u8>>) -> Self {
        let mut normalized = BTreeMap::new();
        for (k, v) in blobs {
            normalized.insert(normalize_rel_path(&k), v);
        }
        let paths = normalized.keys().cloned().collect();
        Self {
            paths,
            blobs: normalized,
        }
    }

    pub fn insert(&mut self, rel_path: impl Into<String>, bytes: Vec<u8>) {
        let key = normalize_rel_path(&rel_path.into());
        if !self.blobs.contains_key(&key) {
            self.paths.push(key.clone());
            self.paths.sort();
        }
        self.blobs.insert(key, bytes);
    }

    pub fn remove(&mut self, rel_path: &str) -> Option<Vec<u8>> {
        let key = normalize_rel_path(rel_path);
        let removed = self.blobs.remove(&key);
        if removed.is_some() {
            self.paths.retain(|p| p != &key);
        }
        removed
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Walk a repository root on disk, respecting `.gitignore` and skipping virtualenvs/caches,
    /// and load relevant source/test/config/asset files in parallel.
    #[cfg(feature = "fs")]
    pub fn load_from_fs(root: &std::path::Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        use ignore::WalkBuilder;
        use rayon::prelude::*;

        let canonical_root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize repo root {}", root.display()))?;

        let mut candidate_paths = Vec::new();
        let walker = WalkBuilder::new(&canonical_root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !(entry.depth() > 0
                    && matches!(
                        name.as_ref(),
                        ".git"
                            | ".venv"
                            | ".venv-min"
                            | "venv"
                            | "node_modules"
                            | "__pycache__"
                            | ".pytest_cache"
                            | ".ruff_cache"
                            | ".mypy_cache"
                            | ".tox"
                            | "htmlcov"
                            | "site"
                            | "dist"
                            | "build"
                            | "target"
                            | ".cache"
                            | ".vault"
                            | "tmp"
                            | "artifacts"
                            | ".idea"
                            | ".vscode"
                    ))
            })
            .build();

        for entry in walker.flatten() {
            if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let rel = match path.strip_prefix(&canonical_root) {
                Ok(r) => normalize_rel_path(&r.to_string_lossy()),
                Err(_) => continue,
            };
            if should_index_file(&rel) {
                candidate_paths.push((rel, path.to_path_buf()));
            }
        }

        let loaded: Vec<(String, Vec<u8>)> = candidate_paths
            .into_par_iter()
            .filter_map(|(rel, abs)| {
                let bytes = std::fs::read(&abs).ok()?;
                Some((rel, bytes))
            })
            .collect();

        let mut blobs = BTreeMap::new();
        for (rel, bytes) in loaded {
            blobs.insert(rel, bytes);
        }
        Ok(Self::from_map(blobs))
    }
}

/// Type alias matching the filesystem-loaded `TreeSource` name expected by host consumers.
#[cfg(feature = "fs")]
pub type FsTreeSource = InMemoryTree;

impl TreeSource for InMemoryTree {
    fn paths(&self) -> &[String] {
        &self.paths
    }

    fn get(&self, rel_path: &str) -> Option<&[u8]> {
        self.blobs.get(rel_path).map(Vec::as_slice)
    }
}

/// Normalize path separators to POSIX `/` and strip leading `./`.
#[must_use]
pub fn normalize_rel_path(path: &str) -> String {
    let s = path.replace('\\', "/");
    s.trim_start_matches("./").to_string()
}

/// Filter deciding which repository files are indexed into `InMemoryTree::load_from_fs`.
#[must_use]
pub fn should_index_file(rel: &str) -> bool {
    if rel.ends_with(".py") || rel.ends_with(".pyi") {
        return true;
    }
    let file_name = rel.rsplit('/').next().unwrap_or(rel);
    if matches!(
        file_name,
        "pyproject.toml"
            | "uv.lock"
            | "pytest.ini"
            | "setup.cfg"
            | "setup.py"
            | ".python-version"
            | "Makefile"
            | "README.md"
            | "CONTRIBUTING.md"
            | "LICENSE"
            | ".pyrn.toml"
            | "pyrn.toml"
            | "zensical.toml"
            | "mkdocs.yml"
            | "Dockerfile"
            | "docker-compose.yml"
    ) {
        return true;
    }
    if rel.starts_with(".github/")
        || rel.starts_with("mk/")
        || rel.starts_with("k8s/")
        || rel.starts_with("docs/")
        || rel.starts_with("tests/")
        || rel.starts_with("testproject/templates/")
        || rel.starts_with("src/")
        || rel.starts_with("qualification/")
    {
        if rel.ends_with(".png")
            || rel.ends_with(".jpg")
            || rel.ends_with(".jpeg")
            || rel.ends_with(".gif")
            || rel.ends_with(".ico")
            || rel.ends_with(".woff")
            || rel.ends_with(".woff2")
            || rel.ends_with(".pdf")
            || rel.ends_with(".sqlite3")
            || rel.ends_with(".db")
        {
            return false;
        }
        return true;
    }
    false
}

/// Compute a domain-separated 64-hex BLAKE3 digest.
#[must_use]
pub fn blake3_domain_hex(domain: &str, data: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(b"\0");
    hasher.update(data);
    hasher.finalize().to_hex().to_string()
}

/// Domain prefix for raw file content blobs.
pub const DOMAIN_RAW_BLOB: &str = "pyrn.blob.raw.v1";
/// Domain prefix for AST-canonicalized Python files (comment/whitespace invariant).
pub const DOMAIN_AST_BLOB: &str = "pyrn.blob.ast.v1";
/// Domain prefix for AST-canonicalized top-level symbols.
pub const DOMAIN_AST_SYMBOL: &str = "pyrn.symbol.ast.v1";
/// Domain prefix for AST-canonicalized module preambles.
pub const DOMAIN_AST_PREAMBLE: &str = "pyrn.preamble.ast.v1";
/// Domain prefix for per-test content-addressed cache keys.
pub const DOMAIN_TEST_KEY: &str = "pyrn.test-key.v1";
/// Domain prefix for environment fingerprint.
pub const DOMAIN_ENV_FP: &str = "pyrn.env-fp.v1";
/// Domain prefix for project fingerprint.
pub const DOMAIN_PROJECT_FP: &str = "pyrn.project-fp.v1";
