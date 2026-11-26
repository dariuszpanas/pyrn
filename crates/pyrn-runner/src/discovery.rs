//! Fast Rust test discovery, static resource-affinity classification,
//! BLAKE3 schema/code epoch fingerprinting, and content-hash + deletion invalidation.

use crate::protocol::ResourceLane;
use anyhow::{Context, Result};
use pyrn_core::config::PyrnConfig;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Discovered Python test file with static affinity and weight heuristics.
#[derive(Clone, Debug)]
pub struct DiscoveredFile {
    /// Repository-relative POSIX path (e.g. `tests/unit/test_settings.py`).
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub lane: ResourceLane,
    pub estimated_tests: usize,
    pub line_count: usize,
    pub has_module_fixture: bool,
}

const DB_INDICATORS: &[&str] = &[
    "django_db",
    "transactional_db",
    "django_db_reset_sequences",
    "django_db_serialized_rollback",
    "admin_client",
    "admin_user",
    "live_server",
    "MigrationExecutor",
    "TransactionTestCase",
    "TestCase)",
    "(TestCase",
    "(db)",
    "(db,",
    ", db)",
    ", db,",
    "db:",
];

const EXTERNAL_INDICATORS: &[&str] = &[
    "@pytest.mark.real_ray",
    "pytest.mark.real_ray",
    "@pytest.mark.live_cluster",
    "pytest.mark.live_cluster",
    "@pytest.mark.postgresql",
    "pytest.mark.postgresql",
];

const MODULE_FIXTURE_INDICATORS: &[&str] = &[
    "scope=\"module\"",
    "scope='module'",
    "scope=\"package\"",
    "scope='package'",
    "scope=\"session\"",
    "scope='session'",
    "def setup_module",
    "def teardown_module",
];

/// Normalize path to forward-slash repository-relative string.
#[must_use]
pub fn to_rel_posix(project_root: &Path, abs_path: &Path) -> String {
    let rel = abs_path.strip_prefix(project_root).unwrap_or(abs_path);
    rel.to_string_lossy().replace('\\', "/")
}

/// Discover test files under `targets` (files or directories) honoring `PyrnConfig`
/// (`testpaths` and `python_files`, e.g. `test_*.py` AND `*_test.py`), and classify each statically.
pub fn discover_test_files(
    project_root: &Path,
    targets: &[String],
    exclude_markers: Option<&str>,
    config: &PyrnConfig,
) -> Result<Vec<DiscoveredFile>> {
    let mut seen = BTreeMap::<String, PathBuf>::new();
    let effective_targets: Vec<String> = if targets.is_empty() {
        if config.testpaths.is_empty() {
            vec!["tests".to_owned()]
        } else {
            config.testpaths.clone()
        }
    } else {
        targets.to_vec()
    };

    for raw in &effective_targets {
        let p = PathBuf::from(raw);
        let full = if p.is_absolute() {
            p
        } else {
            project_root.join(&p)
        };
        if full.is_file() {
            let rel = to_rel_posix(project_root, &full);
            if full.extension().and_then(|e| e.to_str()) == Some("py") {
                seen.insert(rel, full);
            }
            continue;
        }
        if full.is_dir() {
            for entry in WalkDir::new(&full)
                .into_iter()
                .filter_entry(|e| !is_ignored_dir(e.file_name().to_str().unwrap_or("")))
                .flatten()
            {
                if entry.file_type().is_file() {
                    let rel = to_rel_posix(project_root, entry.path());
                    if config.matches_test_filename(&rel) {
                        seen.insert(rel, entry.path().to_path_buf());
                    }
                }
            }
        }
    }

    let excludes_external = exclude_markers
        .map(|m| {
            m.contains("not real_ray")
                && m.contains("not live_cluster")
                && m.contains("not postgresql")
        })
        .unwrap_or(false);

    let mut out = Vec::with_capacity(seen.len());
    for (rel_path, abs_path) in seen {
        let content = fs::read_to_string(&abs_path)
            .with_context(|| format!("read discovered test file {}", abs_path.display()))?;
        let line_count = content.lines().count();
        let estimated_tests = estimate_test_count(&content);
        let has_module_fixture = MODULE_FIXTURE_INDICATORS
            .iter()
            .any(|pat| content.contains(pat));

        let has_external = EXTERNAL_INDICATORS.iter().any(|pat| content.contains(pat));
        let has_db = DB_INDICATORS.iter().any(|pat| content.contains(pat));

        let lane = if has_external && !excludes_external {
            ResourceLane::SerialExternal
        } else if has_db {
            ResourceLane::SqliteDjango
        } else {
            ResourceLane::Hermetic
        };

        out.push(DiscoveredFile {
            rel_path,
            abs_path,
            lane,
            estimated_tests,
            line_count,
            has_module_fixture,
        });
    }

    Ok(out)
}

fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".venv"
            | ".venv-min"
            | "__pycache__"
            | ".pytest_cache"
            | ".ruff_cache"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "htmlcov"
            | ".cache"
    )
}

fn estimate_test_count(content: &str) -> usize {
    let mut defs = 0usize;
    let mut param_boost = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("def test_") || trimmed.starts_with("async def test_") {
            defs += 1;
        } else if trimmed.starts_with("@pytest.mark.parametrize") {
            param_boost += 4;
        }
    }
    (defs + param_boost).max(1)
}

/// Compute deterministic BLAKE3 fingerprint of Django migrations, models, settings, and conftest
/// to safely key both `:memory:` and file-backed SQLite template snapshots.
#[must_use]
pub fn compute_migrations_hash(project_root: &Path, python_exe: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"pyrn.sqlite-template.v2\0");
    hasher.update(python_exe.as_bytes());
    hasher.update(b"\0");

    let mut files = Vec::new();
    for entry in WalkDir::new(project_root)
        .max_depth(6)
        .into_iter()
        .filter_entry(|e| !is_ignored_dir(e.file_name().to_str().unwrap_or("")))
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = to_rel_posix(project_root, path);
        let relevant = rel.contains("/migrations/") && rel.ends_with(".py")
            || rel.ends_with("/models.py")
            || rel.ends_with("conftest.py")
            || rel.ends_with("pyproject.toml")
            || rel.ends_with(".pyrn.toml")
            || rel.ends_with("settings.py");
        if relevant {
            files.push((rel, path.to_path_buf()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for (rel, abs) in files {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        if let Ok(bytes) = fs::read(&abs) {
            hasher.update(&bytes);
        }
        hasher.update(b"\0");
    }

    hasher.finalize().to_hex().to_string()
}

/// Per-file metadata + BLAKE3 content digest so even same-second, same-size edits are caught 100%.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContentSig {
    pub mtime_ns: u128,
    pub size: u64,
    pub content_hash: [u8; 32],
}

/// Snapshot across source, config, and test `.py` files to detect edits and deletions
/// for warm daemon module invalidation vs full worker recycle.
#[derive(Clone, Debug, Default)]
pub struct RepoFingerprint {
    /// Content-hashed core epoch covering non-test `.py` files, `conftest.py`, `pyproject.toml`,
    /// `.pyrn.toml`, `pytest.ini`, and lockfiles. Any change forces warm worker pool recycle.
    pub core_epoch: String,
    /// Per-test-file `(mtime_ns, size, content_blake3)` map for incremental module invalidation.
    pub test_files: BTreeMap<String, FileContentSig>,
}

/// Scan the repository with BLAKE3 content verification (catches equal-`(mtime_ns, size)` edits
/// and respects `PyrnConfig.python_files` globs like `test_*.py` and `*_test.py`).
#[must_use]
pub fn scan_repo_fingerprint(project_root: &Path, config: &PyrnConfig) -> RepoFingerprint {
    let mut core_entries: Vec<(String, [u8; 32])> = Vec::new();
    let mut test_files = BTreeMap::new();

    for entry in WalkDir::new(project_root)
        .max_depth(7)
        .into_iter()
        .filter_entry(|e| !is_ignored_dir(e.file_name().to_str().unwrap_or("")))
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = to_rel_posix(project_root, path);
        let is_py = rel.ends_with(".py");
        let is_cfg = rel == "pyproject.toml"
            || rel == ".pyrn.toml"
            || rel == "pyrn.toml"
            || rel == "pytest.ini"
            || rel == "uv.lock";
        if !is_py && !is_cfg {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let size = meta.len();
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let content_hash = *blake3::hash(&bytes).as_bytes();

        let is_conftest = rel == "conftest.py" || rel.ends_with("/conftest.py");
        let is_test_case_file = is_py && !is_conftest && config.matches_test_filename(&rel);

        if is_test_case_file {
            test_files.insert(
                rel,
                FileContentSig {
                    mtime_ns,
                    size,
                    content_hash,
                },
            );
        } else {
            core_entries.push((rel, content_hash));
        }
    }

    core_entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = blake3::Hasher::new();
    h.update(b"pyrn.core-epoch.v2\0");
    for (rel, digest) in core_entries {
        h.update(rel.as_bytes());
        h.update(b"\0");
        h.update(&digest);
    }

    RepoFingerprint {
        core_epoch: h.finalize().to_hex().to_string(),
        test_files,
    }
}

/// Return repository-relative test paths that were **added**, **content-modified** (even with
/// identical `(mtime_ns, size)`), or **deleted** between snapshots.
#[must_use]
pub fn diff_changed_test_files(prev: &RepoFingerprint, curr: &RepoFingerprint) -> Vec<String> {
    let mut changed = Vec::new();
    for (path, sig) in &curr.test_files {
        match prev.test_files.get(path) {
            Some(old) if old.content_hash == sig.content_hash => {}
            _ => changed.push(path.clone()),
        }
    }
    for path in prev.test_files.keys() {
        if !curr.test_files.contains_key(path) {
            changed.push(path.clone());
        }
    }
    changed.sort();
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_both_prefix_and_suffix_test_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("test_alpha.py"), "def test_a(): pass\n").unwrap();
        fs::write(dir.path().join("beta_test.py"), "def test_b(): pass\n").unwrap();
        fs::write(dir.path().join("helper.py"), "def util(): pass\n").unwrap();

        let cfg = PyrnConfig {
            testpaths: vec![".".to_owned()],
            python_files: vec!["test_*.py".to_owned(), "*_test.py".to_owned()],
            ..PyrnConfig::default()
        };
        let files = discover_test_files(dir.path(), &[], None, &cfg).expect("discover");
        let names: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(names, vec!["beta_test.py", "test_alpha.py"]);
    }

    #[test]
    fn diff_detects_same_size_same_mtime_edit_and_deletion() {
        let mut prev = RepoFingerprint::default();
        let mut curr = RepoFingerprint::default();

        let h1 = *blake3::hash(b"assert 1 == 1").as_bytes();
        let h2 = *blake3::hash(b"assert 1 == 2").as_bytes();
        let h3 = *blake3::hash(b"assert True").as_bytes();

        prev.test_files.insert(
            "tests/test_edit.py".to_owned(),
            FileContentSig {
                mtime_ns: 12345,
                size: 13,
                content_hash: h1,
            },
        );
        prev.test_files.insert(
            "tests/deleted_test.py".to_owned(),
            FileContentSig {
                mtime_ns: 12345,
                size: 11,
                content_hash: h3,
            },
        );

        // Same mtime_ns and same size, different content hash!
        curr.test_files.insert(
            "tests/test_edit.py".to_owned(),
            FileContentSig {
                mtime_ns: 12345,
                size: 13,
                content_hash: h2,
            },
        );

        let diff = diff_changed_test_files(&prev, &curr);
        assert_eq!(
            diff,
            vec![
                "tests/deleted_test.py".to_owned(),
                "tests/test_edit.py".to_owned()
            ]
        );
    }
}
