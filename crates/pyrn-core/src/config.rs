//! Pure configuration loader for `.pyrn.toml` / `pyrn.toml` and `pyproject.toml`.
//!
//! Operates purely over `TreeSource` in the default pure in-memory build (zero filesystem I/O),
//! with an optional `#[cfg(feature = "fs")]` directory helper.

use serde::{Deserialize, Serialize};

use crate::tree::TreeSource;

/// How `conftest.py` function-local imports are scoped in TIA.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConftestPolicy {
    /// Top-level imports + `pytest_plugins` apply to all subtree tests; non-autouse fixtures apply
    /// only to requesting tests; DB-conditional autouse fixtures apply only to DB tests;
    /// pure attribute-reset `try: import mod; mod._x = None` cache clearers do not fan out globally.
    #[default]
    SmartFixtureScoped,
    /// Every import anywhere in `conftest.py` fans out to all subtree tests.
    StrictAllImports,
}

/// Fail-closed policy for changed files matching neither a Python module nor a known asset edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownPathPolicy {
    /// Fail closed: run full suite whenever an unmapped file changes.
    #[default]
    Full,
    /// Only run tests with explicit static/dynamic asset edges.
    MappedOnly,
}

/// Unified project configuration merged from `pyproject.toml` (`[tool.pytest.ini_options]`,
/// `[tool.pyrn]`, `[tool.hatch.build.targets.wheel]`) and `.pyrn.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyrnConfig {
    /// Test root directories (default: `["tests"]` if present, else repo root).
    pub testpaths: Vec<String>,
    /// Glob patterns matching test filenames (default: `["test_*.py", "*_test.py"]`).
    pub python_files: Vec<String>,
    /// Glob patterns matching test class names (default: `["Test*"]`).
    pub python_classes: Vec<String>,
    /// Glob patterns matching test function/method names (default: `["test_*"]`).
    pub python_functions: Vec<String>,
    /// Python package source roots (e.g., `["src"]`, detected from hatch/setuptools or auto-detected).
    pub source_roots: Vec<String>,
    /// Opt-in project autouse fixtures verified to be pure/DB-conditional guards (zero hardcoding in engine!).
    pub pure_autouse_guards: Vec<String>,
    /// Optional hard cap on worker count (`[runner].max_workers` or top-level `max_workers`).
    pub max_workers: Option<usize>,
    /// Estimated per-worker memory budget in MiB for container OOM protection (default 300 MiB).
    pub worker_mem_mb: Option<u64>,
    /// Environment variable prefixes/keys always hashed into the cache key (default: `["DJANGO_", "RAY_"]`).
    pub env_allowlist_prefixes: Vec<String>,
    /// Extra repository-relative paths whose modification forces `FullVerified`.
    pub extra_full_triggers: Vec<String>,
    /// Default conftest scoping policy for `StrictImpacted`.
    pub conftest_policy: ConftestPolicy,
    /// Fail-closed policy for unmapped files.
    pub unknown_path_policy: UnknownPathPolicy,
    /// Enable symbol-level refinement under `StrictImpacted` (default: `true`).
    pub symbol_level_tia: bool,
}

impl Default for PyrnConfig {
    fn default() -> Self {
        Self {
            testpaths: vec!["tests".to_string()],
            python_files: vec!["test_*.py".to_string(), "*_test.py".to_string()],
            python_classes: vec!["Test*".to_string()],
            python_functions: vec!["test_*".to_string()],
            source_roots: Vec::new(),
            pure_autouse_guards: Vec::new(),
            max_workers: None,
            worker_mem_mb: None,
            env_allowlist_prefixes: vec!["DJANGO_".to_string(), "RAY_".to_string()],
            extra_full_triggers: Vec::new(),
            conftest_policy: ConftestPolicy::SmartFixtureScoped,
            unknown_path_policy: UnknownPathPolicy::Full,
            symbol_level_tia: true,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct PyrnTomlFile {
    #[serde(default)]
    testpaths: Option<Vec<String>>,
    #[serde(default)]
    python_files: Option<Vec<String>>,
    #[serde(default)]
    python_classes: Option<Vec<String>>,
    #[serde(default)]
    python_functions: Option<Vec<String>>,
    #[serde(default)]
    pure_autouse_guards: Option<Vec<String>>,
    #[serde(default)]
    max_workers: Option<usize>,
    #[serde(default)]
    worker_mem_mb: Option<u64>,
    #[serde(default)]
    env_allowlist_prefixes: Option<Vec<String>>,
    #[serde(default)]
    extra_full_triggers: Option<Vec<String>>,
    #[serde(default)]
    conftest_policy: Option<ConftestPolicy>,
    #[serde(default)]
    unknown_path_policy: Option<UnknownPathPolicy>,
    #[serde(default)]
    symbol_level_tia: Option<bool>,
    #[serde(default)]
    runner: Option<PyrnRunnerSection>,
    #[serde(default)]
    tia: Option<PyrnTiaSection>,
}

#[derive(Debug, Default, Deserialize)]
struct PyrnRunnerSection {
    #[serde(default)]
    pure_autouse_guards: Option<Vec<String>>,
    #[serde(default)]
    max_workers: Option<usize>,
    #[serde(default)]
    worker_mem_mb: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct PyrnTiaSection {
    #[serde(default)]
    conftest_policy: Option<ConftestPolicy>,
    #[serde(default)]
    unknown_path_policy: Option<UnknownPathPolicy>,
    #[serde(default)]
    symbol_level_tia: Option<bool>,
    #[serde(default)]
    env_allowlist_prefixes: Option<Vec<String>>,
    #[serde(default)]
    extra_full_triggers: Option<Vec<String>>,
}

impl PyrnConfig {
    /// Load and merge configuration purely from a `TreeSource` (`pyproject.toml` + `.pyrn.toml` / `pyrn.toml`).
    #[must_use]
    pub fn from_tree(tree: &dyn TreeSource) -> Self {
        let mut cfg = Self::default();

        // 1. Parse `pyproject.toml` ([tool.pytest.ini_options], [tool.hatch], [tool.pyrn])
        if let Some(bytes) = tree.get("pyproject.toml")
            && let Ok(text) = std::str::from_utf8(bytes)
        {
            cfg.apply_pyproject_toml(text);
        }

        // 2. Parse `.pyrn.toml` or `pyrn.toml` (overrides pyproject defaults)
        for candidate in [".pyrn.toml", "pyrn.toml"] {
            if let Some(bytes) = tree.get(candidate)
                && let Ok(text) = std::str::from_utf8(bytes)
            {
                cfg.apply_pyrn_toml(text);
            }
        }

        cfg
    }

    /// Convenience helper when loading from a directory on disk (`fs` feature).
    #[cfg(feature = "fs")]
    #[must_use]
    pub fn load_from_dir(root: &std::path::Path) -> Self {
        let mut cfg = Self::default();
        let pyproj = root.join("pyproject.toml");
        if let Ok(text) = std::fs::read_to_string(pyproj) {
            cfg.apply_pyproject_toml(&text);
        }
        for candidate in [".pyrn.toml", "pyrn.toml"] {
            let p = root.join(candidate);
            if let Ok(text) = std::fs::read_to_string(p) {
                cfg.apply_pyrn_toml(&text);
            }
        }
        cfg
    }

    /// Parse relevant sections from a `pyproject.toml` string.
    pub fn apply_pyproject_toml(&mut self, text: &str) {
        let Ok(val) = toml::from_str::<toml::Value>(text) else {
            return;
        };
        if let Some(ini) = val
            .get("tool")
            .and_then(|t| t.get("pytest"))
            .and_then(|p| p.get("ini_options"))
        {
            if let Some(v) = extract_str_list(ini.get("testpaths")) {
                self.testpaths = v;
            }
            if let Some(v) = extract_str_list(ini.get("python_files")) {
                self.python_files = v;
            }
            if let Some(v) = extract_str_list(ini.get("python_classes")) {
                self.python_classes = v;
            }
            if let Some(v) = extract_str_list(ini.get("python_functions")) {
                self.python_functions = v;
            }
        }
        if let Some(pkgs) = val
            .get("tool")
            .and_then(|t| t.get("hatch"))
            .and_then(|h| h.get("build"))
            .and_then(|b| b.get("targets"))
            .and_then(|t| t.get("wheel"))
            .and_then(|w| w.get("packages"))
            .and_then(|p| p.as_array())
        {
            for pkg in pkgs {
                if let Some(s) = pkg.as_str()
                    && let Some((prefix, _)) = s.rsplit_once('/')
                    && !self.source_roots.iter().any(|r| r == prefix)
                {
                    self.source_roots.push(prefix.to_string());
                }
            }
        }
        if let Some(tool_pyrn) = val.get("tool").and_then(|t| t.get("pyrn"))
            && let Ok(parsed) = tool_pyrn.clone().try_into::<PyrnTomlFile>()
        {
            self.merge_toml_struct(parsed);
        }
    }

    /// Parse a standalone `.pyrn.toml` / `pyrn.toml` string.
    pub fn apply_pyrn_toml(&mut self, text: &str) {
        if let Ok(parsed) = toml::from_str::<PyrnTomlFile>(text) {
            self.merge_toml_struct(parsed);
        }
    }

    fn merge_toml_struct(&mut self, parsed: PyrnTomlFile) {
        if let Some(v) = parsed.testpaths {
            self.testpaths = v;
        }
        if let Some(v) = parsed.python_files {
            self.python_files = v;
        }
        if let Some(v) = parsed.python_classes {
            self.python_classes = v;
        }
        if let Some(v) = parsed.python_functions {
            self.python_functions = v;
        }
        if let Some(v) = parsed.pure_autouse_guards {
            self.pure_autouse_guards = v;
        }
        if let Some(v) = parsed.max_workers {
            self.max_workers = Some(v);
        }
        if let Some(v) = parsed.worker_mem_mb {
            self.worker_mem_mb = Some(v);
        }
        if let Some(v) = parsed.env_allowlist_prefixes {
            self.env_allowlist_prefixes = v;
        }
        if let Some(v) = parsed.extra_full_triggers {
            self.extra_full_triggers = v;
        }
        if let Some(v) = parsed.conftest_policy {
            self.conftest_policy = v;
        }
        if let Some(v) = parsed.unknown_path_policy {
            self.unknown_path_policy = v;
        }
        if let Some(v) = parsed.symbol_level_tia {
            self.symbol_level_tia = v;
        }
        if let Some(runner) = parsed.runner {
            if let Some(v) = runner.pure_autouse_guards {
                self.pure_autouse_guards = v;
            }
            if let Some(v) = runner.max_workers {
                self.max_workers = Some(v);
            }
            if let Some(v) = runner.worker_mem_mb {
                self.worker_mem_mb = Some(v);
            }
        }
        if let Some(tia) = parsed.tia {
            if let Some(v) = tia.conftest_policy {
                self.conftest_policy = v;
            }
            if let Some(v) = tia.unknown_path_policy {
                self.unknown_path_policy = v;
            }
            if let Some(v) = tia.symbol_level_tia {
                self.symbol_level_tia = v;
            }
            if let Some(v) = tia.env_allowlist_prefixes {
                self.env_allowlist_prefixes = v;
            }
            if let Some(v) = tia.extra_full_triggers {
                self.extra_full_triggers = v;
            }
        }
    }

    /// Check whether a repository-relative path matches `python_files` (e.g. `test_*.py`, `*_test.py`).
    #[must_use]
    pub fn matches_test_filename(&self, rel_path: &str) -> bool {
        let fname = rel_path.rsplit('/').next().unwrap_or(rel_path);
        self.python_files
            .iter()
            .any(|pat| simple_glob_match(pat, fname))
    }

    /// Check whether a class name matches `python_classes` (default `Test*`).
    #[must_use]
    pub fn matches_test_class(&self, class_name: &str) -> bool {
        self.python_classes
            .iter()
            .any(|pat| simple_glob_match(pat, class_name))
    }

    /// Check whether a function/method name matches `python_functions` (default `test_*`).
    #[must_use]
    pub fn matches_test_function(&self, fn_name: &str) -> bool {
        self.python_functions
            .iter()
            .any(|pat| simple_glob_match(pat, fn_name))
    }
}

fn extract_str_list(val: Option<&toml::Value>) -> Option<Vec<String>> {
    match val? {
        toml::Value::Array(arr) => {
            let items: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect();
            if items.is_empty() { None } else { Some(items) }
        }
        toml::Value::String(s) => {
            let items: Vec<String> = s.split_whitespace().map(ToOwned::to_owned).collect();
            if items.is_empty() { None } else { Some(items) }
        }
        _ => None,
    }
}

/// Simple wildcard (`*` and `?`) matcher for pytest `python_files`/`python_classes`/`python_functions`.
#[must_use]
pub fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            pi += 1;
            star_t = ti;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}
