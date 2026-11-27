//! Symbol-aware dependency graph, impact analysis, and safety classification.
//!
//! Pure over `TreeSource` so the exact same engine executes:
//! 1. In-process inside host services or library embedders directly over in-memory trees (zero checkout).
//! 2. Locally via `pyrn-runner` and `pyrn-cli`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::config::{ConftestPolicy, PyrnConfig, UnknownPathPolicy};
use crate::dynamic::{DynamicTraceIndex, base_nodeid};
use crate::parser::{ChangedSymbols, ParsedPyFile, SymbolKind, TestNode, parse_python_file};
use crate::receipt::ConfidenceTier;
use crate::tree::{
    DOMAIN_ENV_FP, DOMAIN_PROJECT_FP, DOMAIN_RAW_BLOB, TreeSource, blake3_domain_hex,
    normalize_rel_path,
};

/// Safety classification for content-addressed caching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CacheSafety {
    /// Pure/hermetic or in-memory SQLite test whose inputs are completely captured by the closure hash.
    DeterministicCacheable,
    /// Deterministic conditional on explicit environment variable `(key, value)` hashes.
    EnvDependentCacheable { env_keys: Vec<String> },
    /// Impure/external/resource-owning test (`real_ray`, `live_cluster`, `postgresql`, network, subprocess).
    /// NEVER served from cache; always executes when selected or in full runs.
    TaintedAlwaysRun { reasons: Vec<String> },
}

impl CacheSafety {
    #[must_use]
    pub fn is_tainted(&self) -> bool {
        matches!(self, CacheSafety::TaintedAlwaysRun { .. })
    }
}

/// Typed dependency edge reason for explainability and auditability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    StaticImport,
    LazyImport,
    PackageInit,
    StringModuleRef,
    ConftestFile,
    ConftestTopImport,
    ConftestPlugin,
    FixtureDep,
    DjangoDbModels,
    DjangoDbMigrations,
    DjangoSettings,
    DjangoAdmin,
    DjangoUrlsTemplates,
    DjangoManagementCommand,
    RepoAssetLiteral,
    RepoAssetDirPrefix,
    DynamicTracePy,
    DynamicTraceAsset,
}

/// Symbol specificity on a dependency edge `caller -> target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolFilter {
    /// Whole module depended upon (`import M`, `from M import *`, `StringModuleRef`, etc.).
    All,
    /// Caller explicitly imports ONLY these named symbols (`from M import a, b`), plus preamble.
    Named(BTreeSet<String>),
    /// Caller only triggers import-time execution of `M`'s preamble (e.g. ancestor `PackageInit`
    /// when importing a sibling submodule, or `from pkg import submod`).
    PreambleOnly,
}

impl SymbolFilter {
    pub fn merge(&mut self, other: SymbolFilter) {
        match (&mut *self, other) {
            (SymbolFilter::All, _) => {}
            (_, SymbolFilter::All) => *self = SymbolFilter::All,
            (SymbolFilter::PreambleOnly, rhs) => *self = rhs,
            (SymbolFilter::Named(_), SymbolFilter::PreambleOnly) => {}
            (SymbolFilter::Named(lhs), SymbolFilter::Named(rhs)) => lhs.extend(rhs),
        }
    }
}

/// Metadata attached to a directed dependency edge `from -> to`.
#[derive(Debug, Clone)]
pub struct EdgeDetail {
    pub kinds: BTreeSet<EdgeKind>,
    pub symbols: SymbolFilter,
    /// Enclosing top-level symbols in `from` that perform this import (None if module-level/whole-file).
    /// Used for multi-hop symbol propagation: if `from::validate_settings` imports `to::validate_storage_configuration`,
    /// a change to `to::validate_storage_configuration` only affects `from::validate_settings`!
    pub caller_symbols: Option<BTreeSet<String>>,
}

impl Default for EdgeDetail {
    fn default() -> Self {
        Self {
            kinds: BTreeSet::new(),
            symbols: SymbolFilter::PreambleOnly,
            caller_symbols: Some(BTreeSet::new()),
        }
    }
}

impl EdgeDetail {
    pub fn record(&mut self, kind: EdgeKind, filter: SymbolFilter, caller_sym: Option<&str>) {
        let was_empty = self.kinds.is_empty();
        self.kinds.insert(kind);
        if was_empty {
            self.symbols = filter;
        } else {
            self.symbols.merge(filter);
        }
        match (&mut self.caller_symbols, caller_sym) {
            (Some(set), Some(sym)) => {
                set.insert(sym.to_string());
            }
            (_, None) => {
                self.caller_symbols = None;
            }
            (None, _) => {}
        }
    }
}

/// Changed file with optional symbol-level granularity (from AST diff of base vs candidate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFileSpec {
    pub path: String,
    pub symbols: ChangedSymbols,
}

impl ChangedFileSpec {
    #[must_use]
    pub fn all(path: impl Into<String>) -> Self {
        Self {
            path: normalize_rel_path(&path.into()),
            symbols: ChangedSymbols::All,
        }
    }

    #[must_use]
    pub fn with_symbols(path: impl Into<String>, symbols: ChangedSymbols) -> Self {
        Self {
            path: normalize_rel_path(&path.into()),
            symbols,
        }
    }
}

/// Configuration for building the dependency graph and computing impact.
#[derive(Debug, Clone)]
pub struct GraphConfig {
    pub project: PyrnConfig,
    pub conftest_policy: ConftestPolicy,
    pub unknown_path_policy: UnknownPathPolicy,
    pub symbol_level_tia: bool,
    pub platform_tag: String,
    pub python_version: String,
    pub django_settings_module: String,
}

impl Default for GraphConfig {
    fn default() -> Self {
        let project = PyrnConfig::default();
        Self {
            conftest_policy: project.conftest_policy,
            unknown_path_policy: project.unknown_path_policy,
            symbol_level_tia: project.symbol_level_tia,
            project,
            platform_tag: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            python_version: "3.12".to_string(),
            django_settings_module: "default".to_string(),
        }
    }
}

impl GraphConfig {
    /// Build a `GraphConfig` inheriting defaults from a loaded `PyrnConfig`.
    #[must_use]
    pub fn from_project(project: PyrnConfig) -> Self {
        Self {
            conftest_policy: project.conftest_policy,
            unknown_path_policy: project.unknown_path_policy,
            symbol_level_tia: project.symbol_level_tia,
            project,
            ..Self::default()
        }
    }

    /// Enforce confidence-tier soundness invariants:
    /// - `StrictImpacted`: uses configured conftest policy, AST hashing, and symbol-level TIA.
    /// - `TransitiveImpacted` / `FullVerified`: forces `StrictAllImports`, disables symbol filtering,
    ///   and forces raw file hashing.
    #[must_use]
    pub fn effective_for_tier(mut self, tier: ConfidenceTier) -> (Self, bool) {
        match tier {
            ConfidenceTier::StrictImpacted => (self, false),
            ConfidenceTier::TransitiveImpacted | ConfidenceTier::FullVerified => {
                self.conftest_policy = ConftestPolicy::StrictAllImports;
                self.symbol_level_tia = false;
                (self, true)
            }
        }
    }
}

/// Timing breakdown for benchmarking Rust TIA speed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuildTimings {
    pub total_files_indexed: usize,
    pub py_files_parsed: usize,
    pub py_parse_fallbacks: usize,
    pub test_files: usize,
    pub test_nodes: usize,
    pub parse_and_hash_ms: f64,
    pub graph_build_ms: f64,
    pub total_ms: f64,
}

/// Full compiled repository dependency graph and BLAKE3 index.
#[derive(Debug, Clone)]
pub struct DependencyGraph {
    pub config: GraphConfig,
    pub timings: BuildTimings,
    /// `rel_path -> raw BLAKE3 hex` for every indexed file (Python + non-Python assets).
    pub raw_hashes: BTreeMap<String, String>,
    /// Parsed Python file metadata (including per-symbol AST tables).
    pub py_files: BTreeMap<String, ParsedPyFile>,
    /// Dotted Python module name -> `rel_path` (`myapp.admin` -> `src/myapp/admin.py`).
    pub module_to_path: BTreeMap<String, String>,
    /// `rel_path` -> dotted Python module name.
    pub path_to_module: BTreeMap<String, String>,
    /// Forward file-to-file dependencies with edge kinds and symbol specificity.
    pub forward_deps: BTreeMap<String, BTreeMap<String, EdgeDetail>>,
    /// Reverse dependencies: `dep_file -> BTreeSet<dependent_file>`.
    pub reverse_deps: BTreeMap<String, BTreeSet<String>>,
    /// Directory-prefix asset subscriptions: `dir_prefix -> BTreeSet<test_file>`.
    pub dir_watchers: BTreeMap<String, BTreeSet<String>>,
    /// All discovered test nodes.
    pub tests: Vec<TestNode>,
    /// Test file -> indices into `self.tests`.
    pub tests_by_file: BTreeMap<String, Vec<usize>>,
    /// Safety classification per test node (parallel to `self.tests`).
    pub test_safety: Vec<CacheSafety>,
    /// Environment fingerprint (64-hex BLAKE3 of platform + python + lockfile + settings).
    pub env_fingerprint: String,
    /// Project fingerprint (64-hex BLAKE3 of project identity + package roots).
    pub project_fingerprint: String,
    /// Files whose modification triggers conservative full-suite invalidation.
    pub global_trigger_files: BTreeSet<String>,
    /// Dynamic trace files merged (if any).
    pub has_dynamic_traces: bool,
    /// Per-file dynamic duration hints (ms) if dynamic traces or CSV loaded.
    pub file_durations_ms: BTreeMap<String, f64>,
}

/// One impacted test file with explainable dependency reasons.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactedTestFile {
    pub test_file: String,
    pub test_count: usize,
    pub reasons: Vec<String>,
    pub has_tainted_tests: bool,
}

/// Full impact analysis result for a set of changed files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactResult {
    pub requested_tier: ConfidenceTier,
    pub effective_tier: ConfidenceTier,
    pub full_fallback_reasons: Vec<String>,
    pub changed_files: Vec<String>,
    pub semantic_changed_files: Vec<String>,
    pub comment_or_format_only_files: Vec<String>,
    pub symbol_pruned_edges: usize,
    pub total_test_files: usize,
    pub impacted_test_files: usize,
    pub total_test_nodes: usize,
    pub impacted_test_nodes: usize,
    pub tainted_test_nodes: usize,
    pub reduction_pct: f64,
    pub impacted: Vec<ImpactedTestFile>,
    pub eval_ms: f64,
}

impl DependencyGraph {
    /// Build a complete dependency graph purely from a `TreeSource` (plus optional dynamic traces).
    #[must_use]
    pub fn build(
        tree: &dyn TreeSource,
        dynamic: Option<&DynamicTraceIndex>,
        mut config: GraphConfig,
    ) -> Self {
        let total_start = Instant::now();
        let all_paths = tree.paths();

        // Merge project configuration from `pyproject.toml` + `.pyrn.toml` if caller left defaults
        let detected_proj = PyrnConfig::from_tree(tree);
        if config.project.source_roots.is_empty() {
            config.project.source_roots = detected_proj.source_roots;
        }
        if config.project.pure_autouse_guards.is_empty() {
            config.project.pure_autouse_guards = detected_proj.pure_autouse_guards;
        }
        if config.project.extra_full_triggers.is_empty() {
            config.project.extra_full_triggers = detected_proj.extra_full_triggers;
        }
        if config.project.python_files == PyrnConfig::default().python_files {
            config.project.python_files = detected_proj.python_files;
        }
        if config.project.python_classes == PyrnConfig::default().python_classes {
            config.project.python_classes = detected_proj.python_classes;
        }
        if config.project.python_functions == PyrnConfig::default().python_functions {
            config.project.python_functions = detected_proj.python_functions;
        }
        if config.project.testpaths == PyrnConfig::default().testpaths {
            config.project.testpaths = detected_proj.testpaths;
        }

        // 1. Parallel raw hashing + Python 3.12 AST parsing + per-symbol AST table
        let parse_start = Instant::now();
        let proj_ref = &config.project;
        let parsed_results: Vec<(String, String, Option<ParsedPyFile>)> = all_paths
            .par_iter()
            .filter_map(|rel| {
                let bytes = tree.get(rel)?;
                let raw_hash = blake3_domain_hex(DOMAIN_RAW_BLOB, bytes);
                let parsed = if rel.ends_with(".py") {
                    Some(parse_python_file(rel, bytes, &raw_hash, proj_ref))
                } else {
                    None
                };
                Some((rel.clone(), raw_hash, parsed))
            })
            .collect();
        let parse_and_hash_ms = parse_start.elapsed().as_secs_f64() * 1000.0;

        let graph_start = Instant::now();
        let mut raw_hashes = BTreeMap::new();
        let mut py_files = BTreeMap::new();
        let mut py_parse_fallbacks = 0usize;

        for (rel, raw_hash, parsed) in parsed_results {
            raw_hashes.insert(rel.clone(), raw_hash);
            if let Some(p) = parsed {
                if p.parse_fallback {
                    py_parse_fallbacks += 1;
                }
                py_files.insert(rel, p);
            }
        }

        // 2. Discover Python source roots (`src/` layout via pyproject.toml + auto-detect)
        let source_roots = detect_source_roots(&config.project, &py_files);

        // 3. Build module <-> path index
        let mut module_to_path = BTreeMap::new();
        let mut path_to_module = BTreeMap::new();
        for rel in py_files.keys() {
            if let Some(mod_name) = rel_path_to_module(rel, &source_roots) {
                module_to_path
                    .entry(mod_name.clone())
                    .or_insert_with(|| rel.clone());
                path_to_module.insert(rel.clone(), mod_name);
            }
        }

        // 4. Identify Django semantic groups
        let mut django_models_files = BTreeSet::new();
        let mut django_migration_files = BTreeSet::new();
        let mut django_admin_files = BTreeSet::new();
        let mut django_conf_files = BTreeSet::new();
        let mut django_backend_files = BTreeSet::new();
        let mut django_url_view_files = BTreeSet::new();
        let mut django_template_files = BTreeSet::new();
        let mut django_commands: BTreeMap<String, String> = BTreeMap::new();

        for rel in all_paths {
            if rel.ends_with("/models.py") || (rel.contains("/models/") && rel.ends_with(".py")) {
                django_models_files.insert(rel.clone());
            }
            if rel.ends_with("/apps.py") {
                django_models_files.insert(rel.clone());
            }
            if rel.contains("/migrations/") && rel.ends_with(".py") {
                django_migration_files.insert(rel.clone());
            }
            if rel.ends_with("/admin.py") || (rel.contains("/admin/") && rel.ends_with(".py")) {
                django_admin_files.insert(rel.clone());
            }
            if rel.contains("/conf/") && rel.ends_with(".py") {
                django_conf_files.insert(rel.clone());
            }
            if rel.ends_with("/backends.py") {
                django_backend_files.insert(rel.clone());
            }
            if rel.ends_with("/urls.py")
                || rel.ends_with("/views.py")
                || rel.ends_with("/api.py")
                || rel.ends_with("/api_boundary.py")
                || rel.ends_with("/route_policy.py")
            {
                django_url_view_files.insert(rel.clone());
            }
            if rel.contains("/templates/") || rel.contains("/static/") {
                django_template_files.insert(rel.clone());
            }
            if let Some(idx) = rel.find("/management/commands/") {
                let tail = &rel[idx + "/management/commands/".len()..];
                if let Some(stem) = tail.strip_suffix(".py")
                    && !stem.starts_with('_')
                    && !stem.contains('/')
                {
                    django_commands.insert(stem.to_string(), rel.clone());
                }
            }
        }

        let all_dirs: BTreeSet<String> = all_paths.iter().flat_map(|p| ancestor_dirs(p)).collect();

        // 5. Build forward dependency edges with symbol specificity
        let mut forward_deps: BTreeMap<String, BTreeMap<String, EdgeDetail>> = BTreeMap::new();
        let mut dir_watchers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        let add_edge = |fwd: &mut BTreeMap<String, BTreeMap<String, EdgeDetail>>,
                        from: &str,
                        to: &str,
                        kind: EdgeKind,
                        filter: SymbolFilter,
                        caller_sym: Option<&str>| {
            if from == to {
                return;
            }
            fwd.entry(from.to_string())
                .or_default()
                .entry(to.to_string())
                .or_default()
                .record(kind, filter, caller_sym);
        };

        let conftest_files: Vec<String> = py_files
            .keys()
            .filter(|p| is_conftest_path(p))
            .cloned()
            .collect();

        let mut fixture_providers: BTreeMap<String, Vec<(String, Vec<String>)>> = BTreeMap::new();
        for cf in &conftest_files {
            if let Some(parsed) = py_files.get(cf) {
                for f in &parsed.fixtures {
                    fixture_providers
                        .entry(f.name.clone())
                        .or_default()
                        .push((cf.clone(), f.local_import_targets.clone()));
                }
            }
        }

        for (rel, parsed) in &py_files {
            let is_cf = is_conftest_path(rel);
            let current_mod = path_to_module.get(rel).map(String::as_str).unwrap_or("");
            let is_pkg_init = rel.ends_with("/__init__.py") || rel == "__init__.py";

            // 5a. Static & lazy Python imports (with exact symbol filters!)
            for imp in &parsed.imports {
                if is_cf
                    && imp.is_local
                    && config.conftest_policy == ConftestPolicy::SmartFixtureScoped
                    && (imp.is_cache_reset_only || imp.in_fixture_scope_filtered)
                {
                    continue;
                }

                let resolved = resolve_import_targets_with_symbols(
                    imp,
                    current_mod,
                    is_pkg_init,
                    &module_to_path,
                );
                let edge_kind = if is_cf && !imp.is_local {
                    EdgeKind::ConftestTopImport
                } else if imp.is_local {
                    EdgeKind::LazyImport
                } else {
                    EdgeKind::StaticImport
                };
                for (target, sym_filter) in resolved {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        &target,
                        edge_kind,
                        sym_filter,
                        imp.enclosing_symbol.as_deref(),
                    );
                    for init_path in package_init_ancestors(&target, &py_files) {
                        add_edge(
                            &mut forward_deps,
                            rel,
                            &init_path,
                            EdgeKind::PackageInit,
                            SymbolFilter::PreambleOnly,
                            None,
                        );
                    }
                }
            }

            // 5b. `pytest_plugins`
            for plugin_mod in &parsed.pytest_plugins {
                if let Some(target) = resolve_dotted_module(plugin_mod, &module_to_path) {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        &target,
                        EdgeKind::ConftestPlugin,
                        SymbolFilter::All,
                        None,
                    );
                }
            }

            // 5c. String-based dotted module references
            for sref in &parsed.string_module_refs {
                if let Some(target) = resolve_dotted_module_or_prefix(sref, &module_to_path) {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        &target,
                        EdgeKind::StringModuleRef,
                        SymbolFilter::All,
                        None,
                    );
                }
            }

            // 5d. Django `call_command("cmd")`
            for cmd in &parsed.django_call_commands {
                if let Some(target) = django_commands.get(cmd) {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        target,
                        EdgeKind::DjangoManagementCommand,
                        SymbolFilter::All,
                        None,
                    );
                }
            }

            // 5e. Repo-relative non-Python asset & directory literals
            for asset in &parsed.asset_path_refs {
                let norm = normalize_rel_path(asset).trim_matches('/').to_string();
                if norm.is_empty() {
                    continue;
                }
                if raw_hashes.contains_key(&norm) {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        &norm,
                        EdgeKind::RepoAssetLiteral,
                        SymbolFilter::All,
                        None,
                    );
                } else if all_dirs.contains(&norm) && !is_broad_code_root(&norm) {
                    dir_watchers
                        .entry(norm.clone())
                        .or_default()
                        .insert(rel.clone());
                    let prefix = format!("{norm}/");
                    for path in all_paths {
                        if path.starts_with(&prefix) {
                            add_edge(
                                &mut forward_deps,
                                rel,
                                path,
                                EdgeKind::RepoAssetDirPrefix,
                                SymbolFilter::All,
                                None,
                            );
                        }
                    }
                } else if norm.ends_with(".yml")
                    || norm.ends_with(".yaml")
                    || norm.ends_with(".json")
                {
                    let suffix = format!("/{norm}");
                    for path in all_paths {
                        if path == &norm || path.ends_with(&suffix) {
                            add_edge(
                                &mut forward_deps,
                                rel,
                                path,
                                EdgeKind::RepoAssetLiteral,
                                SymbolFilter::All,
                                None,
                            );
                        }
                    }
                }
            }
        }

        // 6. Wire test files to conftest hierarchy, fixtures, and Django semantic dependencies
        let mut tests = Vec::new();
        let mut tests_by_file: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut test_safety = Vec::new();

        for (rel, parsed) in &py_files {
            let in_testpaths = config.project.testpaths.is_empty()
                || config.project.testpaths.iter().any(|tp| {
                    let p = tp.trim_end_matches('/');
                    p.is_empty() || p == "." || rel == p || rel.starts_with(&format!("{p}/"))
                });
            let is_test_file = in_testpaths
                && (!parsed.tests.is_empty() || config.project.matches_test_filename(rel));
            if !is_test_file {
                continue;
            }
            // Register test files even with 0 static `def test_*` items (e.g., inherited
            // `unittest.TestCase` subclasses or `pytest_generate_tests` modules) so TIA
            // (`compute_impact_with_symbols`, `plan_python_tests`) never silently drops them.
            tests_by_file.entry(rel.clone()).or_default();

            for cf in &conftest_files {
                let cf_dir = parent_dir(cf);
                if cf_dir.is_empty()
                    || rel.starts_with(&format!("{cf_dir}/"))
                    || parent_dir(rel) == cf_dir
                {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        cf,
                        EdgeKind::ConftestFile,
                        SymbolFilter::All,
                        None,
                    );
                }
            }

            let mut file_uses_db = parsed.uses_migration_executor || parsed.uses_django_db_module;
            let mut file_uses_admin = parsed.uses_django_admin;
            let mut file_uses_urls_templates = parsed.uses_django_client_or_url;
            let file_uses_settings = parsed.uses_django_settings;
            let mut file_requested_fixtures = BTreeSet::new();

            for t in &parsed.tests {
                if t.uses_django_db {
                    file_uses_db = true;
                }
                if t.uses_django_admin {
                    file_uses_admin = true;
                }
                if t.uses_django_client_or_url {
                    file_uses_urls_templates = true;
                }
                for fx in &t.fixture_params {
                    file_requested_fixtures.insert(fx.clone());
                }
            }
            for fx in &parsed.fixtures {
                for p in &fx.params {
                    file_requested_fixtures.insert(p.clone());
                }
            }

            for fx_name in &file_requested_fixtures {
                if let Some(providers) = fixture_providers.get(fx_name) {
                    for (cf_path, local_mods) in providers {
                        add_edge(
                            &mut forward_deps,
                            rel,
                            cf_path,
                            EdgeKind::FixtureDep,
                            SymbolFilter::All,
                            None,
                        );
                        for m in local_mods {
                            if let Some(target) =
                                resolve_dotted_module_or_prefix(m, &module_to_path)
                            {
                                add_edge(
                                    &mut forward_deps,
                                    rel,
                                    &target,
                                    EdgeKind::FixtureDep,
                                    SymbolFilter::All,
                                    None,
                                );
                            }
                        }
                    }
                }
            }

            let file_uses_postgres = parsed
                .tests
                .iter()
                .any(|t| t.markers.iter().any(|m| m == "postgresql"));
            if file_uses_postgres && raw_hashes.contains_key("tests/postgres_settings.py") {
                add_edge(
                    &mut forward_deps,
                    rel,
                    "tests/postgres_settings.py",
                    EdgeKind::DjangoSettings,
                    SymbolFilter::All,
                    None,
                );
            }
            if file_uses_db {
                for m in &django_models_files {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        m,
                        EdgeKind::DjangoDbModels,
                        SymbolFilter::All,
                        None,
                    );
                }
                for mig in &django_migration_files {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        mig,
                        EdgeKind::DjangoDbMigrations,
                        SymbolFilter::All,
                        None,
                    );
                }
                for s in &django_conf_files {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        s,
                        EdgeKind::DjangoSettings,
                        SymbolFilter::All,
                        None,
                    );
                }
            } else if file_uses_settings {
                for s in &django_conf_files {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        s,
                        EdgeKind::DjangoSettings,
                        SymbolFilter::All,
                        None,
                    );
                }
            }

            if parsed.uses_django_tasks {
                for b in &django_backend_files {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        b,
                        EdgeKind::StaticImport,
                        SymbolFilter::All,
                        None,
                    );
                }
            }

            if file_uses_admin {
                for a in &django_admin_files {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        a,
                        EdgeKind::DjangoAdmin,
                        SymbolFilter::All,
                        None,
                    );
                }
                for t in &django_template_files {
                    if t.contains("admin") {
                        add_edge(
                            &mut forward_deps,
                            rel,
                            t,
                            EdgeKind::DjangoUrlsTemplates,
                            SymbolFilter::All,
                            None,
                        );
                    }
                }
            }

            if file_uses_urls_templates {
                for u in &django_url_view_files {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        u,
                        EdgeKind::DjangoUrlsTemplates,
                        SymbolFilter::All,
                        None,
                    );
                }
                for t in &django_template_files {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        t,
                        EdgeKind::DjangoUrlsTemplates,
                        SymbolFilter::All,
                        None,
                    );
                }
                for s in &django_conf_files {
                    add_edge(
                        &mut forward_deps,
                        rel,
                        s,
                        EdgeKind::DjangoSettings,
                        SymbolFilter::All,
                        None,
                    );
                }
            }

            for t in &parsed.tests {
                let idx = tests.len();
                let mut taint_reasons: BTreeSet<String> = t.taint_reasons.iter().cloned().collect();
                let mut env_keys: BTreeSet<String> = parsed.env_vars_read.iter().cloned().collect();

                if let Some(dyn_idx) = dynamic {
                    let base = base_nodeid(&t.nodeid);
                    if let Some(d) = dyn_idx.by_base_nodeid.get(&base) {
                        for r in &d.taint_reasons {
                            taint_reasons.insert(r.clone());
                        }
                        for k in d.env_vars_read.keys() {
                            env_keys.insert(k.clone());
                        }
                    }
                }

                let safety = if !taint_reasons.is_empty() {
                    CacheSafety::TaintedAlwaysRun {
                        reasons: taint_reasons.into_iter().collect(),
                    }
                } else if !env_keys.is_empty() {
                    CacheSafety::EnvDependentCacheable {
                        env_keys: env_keys.into_iter().collect(),
                    }
                } else {
                    CacheSafety::DeterministicCacheable
                };

                tests.push(t.clone());
                test_safety.push(safety);
                tests_by_file.entry(rel.clone()).or_default().push(idx);
            }
        }

        // 7. Merge dynamic traces (if provided) into forward edges & dir watchers
        let mut has_dynamic_traces = false;
        let mut file_durations_ms = BTreeMap::new();
        if let Some(dyn_idx) = dynamic
            && !dyn_idx.is_empty()
        {
            has_dynamic_traces = true;
            file_durations_ms = dyn_idx.file_durations_ms.clone();
            for (test_file, agg) in &dyn_idx.by_test_file {
                if !py_files.contains_key(test_file) {
                    continue;
                }
                // Group observed `rel_path::qualname` symbols by target file when available
                let mut syms_by_file: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
                for sym_entry in &agg.executed_symbols {
                    if let Some((f, q)) = sym_entry.split_once("::") {
                        let top_sym = q.split('.').next().unwrap_or(q);
                        if !top_sym.is_empty() && !top_sym.starts_with('<') {
                            syms_by_file
                                .entry(normalize_rel_path(f))
                                .or_default()
                                .insert(top_sym.to_string());
                        }
                    }
                }

                for py_dep in &agg.executed_py_files {
                    if py_files.contains_key(py_dep) {
                        let filter = match syms_by_file.get(py_dep) {
                            Some(set) if !set.is_empty() => SymbolFilter::Named(set.clone()),
                            _ => SymbolFilter::All,
                        };
                        add_edge(
                            &mut forward_deps,
                            test_file,
                            py_dep,
                            EdgeKind::DynamicTracePy,
                            filter,
                            None,
                        );
                    }
                }
                for asset_dep in &agg.read_assets {
                    if raw_hashes.contains_key(asset_dep) {
                        add_edge(
                            &mut forward_deps,
                            test_file,
                            asset_dep,
                            EdgeKind::DynamicTraceAsset,
                            SymbolFilter::All,
                            None,
                        );
                    }
                }
                for dir in &agg.listed_dirs {
                    dir_watchers
                        .entry(dir.clone())
                        .or_default()
                        .insert(test_file.clone());
                    let prefix = format!("{dir}/");
                    for path in all_paths {
                        if path.starts_with(&prefix) {
                            add_edge(
                                &mut forward_deps,
                                test_file,
                                path,
                                EdgeKind::DynamicTraceAsset,
                                SymbolFilter::All,
                                None,
                            );
                        }
                    }
                }
            }
        }

        // 8. Build reverse dependency adjacency map
        let mut reverse_deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (from, deps) in &forward_deps {
            for to in deps.keys() {
                reverse_deps
                    .entry(to.clone())
                    .or_default()
                    .insert(from.clone());
            }
        }

        // 9. Global invalidation triggers (fail-closed `FullVerified` triggers).
        //    Inserted unconditionally so deleting or adding a trigger file in a candidate commit
        //    still triggers `FullVerified` even under `UnknownPathPolicy::MappedOnly`.
        let mut global_trigger_files = BTreeSet::new();
        for p in [
            "pyproject.toml",
            "uv.lock",
            "pytest.ini",
            "setup.cfg",
            "setup.py",
            ".python-version",
            ".pyrn.toml",
            "pyrn.toml",
            "conftest.py",
            "tests/conftest.py",
        ] {
            global_trigger_files.insert(p.to_string());
        }
        for extra in &config.project.extra_full_triggers {
            global_trigger_files.insert(normalize_rel_path(extra));
        }

        let env_fingerprint = compute_env_fingerprint(&config, &raw_hashes);
        let project_fingerprint = compute_project_fingerprint(&source_roots, &raw_hashes);

        let graph_build_ms = graph_start.elapsed().as_secs_f64() * 1000.0;
        let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;

        let timings = BuildTimings {
            total_files_indexed: raw_hashes.len(),
            py_files_parsed: py_files.len(),
            py_parse_fallbacks,
            test_files: tests_by_file.len(),
            test_nodes: tests.len(),
            parse_and_hash_ms,
            graph_build_ms,
            total_ms,
        };

        Self {
            config,
            timings,
            raw_hashes,
            py_files,
            module_to_path,
            path_to_module,
            forward_deps,
            reverse_deps,
            dir_watchers,
            tests,
            tests_by_file,
            test_safety,
            env_fingerprint,
            project_fingerprint,
            global_trigger_files,
            has_dynamic_traces,
            file_durations_ms,
        }
    }

    /// Compute the full transitive forward closure for any file (returning each reachable file
    /// paired with the merged `SymbolFilter` requested on it).
    ///
    /// Mathematically dual to `compute_impact_with_symbols`:
    /// - Once a synthetic `DjangoSettings` edge is crossed, traversal continues strictly along
    ///   `StaticImport`/`PackageInit` chains (blocking `LazyImport` chains like `validate_settings -> result_storage.py`).
    /// - Under `symbol_aware = true`, if a dependency `M` is reached ONLY via `SymbolFilter::Named(S)`
    ///   (and never via `SymbolFilter::All`), `M`'s symbol filter stays `Named(S)`, allowing `key.rs`
    ///   to hash only `M.module_preamble_hash + intra_deps(S)`!
    #[must_use]
    pub fn transitive_deps_detailed(
        &self,
        start_file: &str,
        symbol_aware: bool,
    ) -> BTreeMap<String, SymbolFilter> {
        // Maps node -> (only_via_synthetic_settings, merged_symbol_filter)
        let mut visited: BTreeMap<String, (bool, SymbolFilter)> = BTreeMap::new();
        let mut queue = VecDeque::new();
        visited.insert(start_file.to_string(), (false, SymbolFilter::All));
        queue.push_back((start_file.to_string(), false, SymbolFilter::All));

        while let Some((curr, restricted, curr_filter)) = queue.pop_front() {
            let Some(neighbors) = self.forward_deps.get(&curr) else {
                continue;
            };
            let curr_parsed = self.py_files.get(&curr);
            let active_caller_syms: Option<BTreeSet<String>> = match (&curr_filter, curr_parsed) {
                (SymbolFilter::Named(s), Some(p)) if symbol_aware && !p.parse_fallback => {
                    if s.iter().all(|name| p.symbols.contains_key(name)) {
                        let mut seeds = s.clone();
                        for pref in &p.preamble_referenced_names {
                            if p.symbols.contains_key(pref) {
                                seeds.insert(pref.clone());
                            }
                        }
                        Some(p.intra_symbol_deps(&seeds))
                    } else {
                        None
                    }
                }
                (SymbolFilter::PreambleOnly, Some(p)) if symbol_aware && !p.parse_fallback => {
                    let seeds: BTreeSet<String> = p
                        .preamble_referenced_names
                        .iter()
                        .filter(|pref| p.symbols.contains_key(*pref))
                        .cloned()
                        .collect();
                    Some(p.intra_symbol_deps(&seeds))
                }
                _ => None,
            };

            for (next, detail) in neighbors {
                if restricted
                    && !detail.kinds.contains(&EdgeKind::StaticImport)
                    && !detail.kinds.contains(&EdgeKind::PackageInit)
                {
                    continue;
                }
                // If caller `curr` is only needed for `active_caller_syms`, and edge `curr -> next`
                // occurs strictly inside local functions disjoint from `active_caller_syms`, skip!
                if let (Some(active), Some(caller_syms)) =
                    (&active_caller_syms, &detail.caller_symbols)
                    && !caller_syms.is_empty()
                    && caller_syms.is_disjoint(active)
                {
                    continue;
                }

                let edge_is_only_settings =
                    detail.kinds.len() == 1 && detail.kinds.contains(&EdgeKind::DjangoSettings);
                let next_restricted = restricted || edge_is_only_settings;
                let next_filter = if !symbol_aware {
                    SymbolFilter::All
                } else if let (
                    Some(active),
                    Some(curr_p),
                    SymbolFilter::Named(imported_names),
                    Some(next_p),
                ) = (
                    &active_caller_syms,
                    curr_parsed,
                    &detail.symbols,
                    self.py_files.get(next),
                ) && detail.caller_symbols.is_none()
                    && !imported_names.is_empty()
                    && !next_p.parse_fallback
                    && detail.kinds.iter().all(|k| {
                        matches!(
                            k,
                            EdgeKind::StaticImport | EdgeKind::LazyImport | EdgeKind::PackageInit
                        )
                    })
                    && imported_names
                        .iter()
                        .all(|n| next_p.symbols.contains_key(n))
                    && imported_names.iter().all(|n| {
                        matches!(
                            curr_p.symbols.get(n).map(|d| d.kind),
                            Some(SymbolKind::ImportedName)
                        )
                    })
                {
                    let used: BTreeSet<String> =
                        imported_names.intersection(active).cloned().collect();
                    if used.is_empty() {
                        SymbolFilter::PreambleOnly
                    } else {
                        SymbolFilter::Named(used)
                    }
                } else {
                    detail.symbols.clone()
                };

                let needs_visit = match visited.get_mut(next) {
                    None => {
                        visited.insert(next.clone(), (next_restricted, next_filter.clone()));
                        true
                    }
                    Some((prev_restricted, prev_filter)) => {
                        let mut changed = false;
                        if *prev_restricted && !next_restricted {
                            *prev_restricted = false;
                            changed = true;
                        }
                        let before = prev_filter.clone();
                        prev_filter.merge(next_filter.clone());
                        if *prev_filter != before {
                            changed = true;
                        }
                        changed
                    }
                };
                if needs_visit {
                    let (r, f) = visited[next].clone();
                    queue.push_back((next.clone(), r, f));
                }
            }
        }

        visited
            .into_iter()
            .map(|(path, (_r, filter))| (path, filter))
            .collect()
    }

    /// Convenience set of all reachable transitive dependency paths (file-level).
    #[must_use]
    pub fn transitive_deps(&self, start_file: &str) -> BTreeSet<String> {
        self.transitive_deps_detailed(start_file, false)
            .into_keys()
            .collect()
    }

    /// File-level convenience wrapper around `compute_impact_with_symbols`.
    #[must_use]
    pub fn compute_impact(
        &self,
        changed_files: &[String],
        comment_only_files: &BTreeSet<String>,
        requested_tier: ConfidenceTier,
    ) -> ImpactResult {
        let specs: Vec<ChangedFileSpec> = changed_files
            .iter()
            .map(|p| {
                let norm = normalize_rel_path(p);
                let syms = if comment_only_files.contains(&norm) {
                    ChangedSymbols::None
                } else {
                    ChangedSymbols::All
                };
                ChangedFileSpec::with_symbols(norm, syms)
            })
            .collect();
        self.compute_impact_with_symbols(&specs, requested_tier)
    }

    /// Compute impacted tests given changed files with optional symbol-level granularity.
    #[must_use]
    pub fn compute_impact_with_symbols(
        &self,
        changed_specs: &[ChangedFileSpec],
        requested_tier: ConfidenceTier,
    ) -> ImpactResult {
        let start = Instant::now();
        let use_symbol_tia =
            requested_tier == ConfidenceTier::StrictImpacted && self.config.symbol_level_tia;

        let normalized_changed: Vec<String> = changed_specs
            .iter()
            .map(|s| normalize_rel_path(&s.path))
            .collect();

        let mut semantic_specs: Vec<(String, ChangedSymbols)> = Vec::new();
        let mut comment_or_format_only = Vec::new();

        for spec in changed_specs {
            let norm = normalize_rel_path(&spec.path);
            if requested_tier == ConfidenceTier::StrictImpacted
                && spec.symbols == ChangedSymbols::None
            {
                comment_or_format_only.push(norm);
            } else {
                let eff_syms = if use_symbol_tia {
                    spec.symbols.clone()
                } else {
                    ChangedSymbols::All
                };
                semantic_specs.push((norm, eff_syms));
            }
        }

        let semantic_changed: Vec<String> = semantic_specs.iter().map(|(p, _)| p.clone()).collect();
        let mut full_fallback_reasons = Vec::new();
        let mut effective_tier = requested_tier;
        let mut symbol_pruned_edges = 0usize;

        if requested_tier == ConfidenceTier::FullVerified {
            full_fallback_reasons.push("explicitly requested full_verified tier".to_string());
        }

        let mut impacted_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for (changed, changed_syms) in &semantic_specs {
            if self.global_trigger_files.contains(changed) {
                effective_tier = ConfidenceTier::FullVerified;
                full_fallback_reasons.push(format!("global trigger modified: {changed}"));
                continue;
            }
            if is_conftest_path(changed) {
                let cf_dir = parent_dir(changed);
                for tf in self.tests_by_file.keys() {
                    if cf_dir.is_empty()
                        || tf.starts_with(&format!("{cf_dir}/"))
                        || parent_dir(tf) == cf_dir
                    {
                        impacted_map
                            .entry(tf.clone())
                            .or_default()
                            .insert(format!("conftest:{changed}"));
                    }
                }
                continue;
            }

            let mut matched_any = false;
            let mut file_symbol_pruned = 0usize;
            if self.tests_by_file.contains_key(changed) {
                matched_any = true;
                impacted_map
                    .entry(changed.clone())
                    .or_default()
                    .insert("direct_test_edit".to_string());
            }

            // Compute initial affected export set for `changed` (None means full-file `All`)
            let initial_affected: Option<BTreeSet<String>> = match (use_symbol_tia, changed_syms) {
                (true, ChangedSymbols::Some { names }) => self
                    .py_files
                    .get(changed)
                    .and_then(|p| p.affected_exports(names)),
                _ => None,
            };

            // Symbol-aware reverse BFS!
            // State per node: (best_only_static_or_init, Option<affected_exports_set> where None = All)
            #[derive(Clone, PartialEq, Eq)]
            struct VisitState {
                only_static_or_init: bool,
                affected_exports: Option<BTreeSet<String>>,
            }

            let mut visited: BTreeMap<String, VisitState> = BTreeMap::new();
            let mut queue: VecDeque<(String, usize, VisitState)> = VecDeque::new();
            let root_state = VisitState {
                only_static_or_init: true,
                affected_exports: initial_affected,
            };
            visited.insert(changed.clone(), root_state.clone());
            queue.push_back((changed.clone(), 0, root_state));

            while let Some((curr, depth, state)) = queue.pop_front() {
                let Some(dependents) = self.reverse_deps.get(&curr) else {
                    continue;
                };
                let curr_parsed = self.py_files.get(&curr);

                for dep in dependents {
                    let Some(detail) = self.forward_deps.get(dep).and_then(|m| m.get(&curr)) else {
                        continue;
                    };

                    // 1. Symmetric DjangoSettings check
                    let only_synthetic_settings =
                        detail.kinds.len() == 1 && detail.kinds.contains(&EdgeKind::DjangoSettings);
                    if only_synthetic_settings && !state.only_static_or_init {
                        continue;
                    }

                    // 2. Sound symbol-level edge check (when `state.affected_exports` is `Some(aff)`)
                    let mut next_affected: Option<BTreeSet<String>> = None;
                    if let Some(ref aff) = state.affected_exports {
                        match detail.symbols {
                            SymbolFilter::PreambleOnly => {
                                // Pure package-init edge (`from pkg.sibling import ...` triggering
                                // `pkg/__init__.py`'s preamble): when `depth > 0`, `curr` itself was
                                // NOT edited and its preamble does NOT call any affected symbol, so
                                // sibling submodules cannot observe the change -> PRUNE!
                                if depth > 0 {
                                    file_symbol_pruned += 1;
                                    symbol_pruned_edges += 1;
                                    continue;
                                }
                            }
                            SymbolFilter::Named(ref imported_names)
                                if !imported_names.is_empty() =>
                            {
                                let all_known_in_curr = curr_parsed
                                    .map(|p| {
                                        !p.parse_fallback
                                            && imported_names
                                                .iter()
                                                .all(|n| p.symbols.contains_key(n))
                                    })
                                    .unwrap_or(false);
                                if all_known_in_curr && imported_names.is_disjoint(aff) {
                                    // ZERO overlap between affected exports and caller's named imports -> PRUNE!
                                    file_symbol_pruned += 1;
                                    symbol_pruned_edges += 1;
                                    continue;
                                }
                                let only_import_edges = detail.kinds.iter().all(|k| {
                                    matches!(
                                        k,
                                        EdgeKind::StaticImport
                                            | EdgeKind::LazyImport
                                            | EdgeKind::PackageInit
                                    )
                                });
                                // Overlap! Narrow downstream affected exports soundly:
                                // (a) Local import inside named enclosing symbols `caller_syms` ->
                                //     propagate only `dep.affected_exports(caller_syms)`.
                                // (b) Top-level `from curr import x` (re-export or module binding) ->
                                //     if every overlapping name is a known `ImportedName` in `dep`,
                                //     propagate `dep.affected_exports(&overlap)` (automatically fails
                                //     closed to `None` if `dep`'s preamble calls any overlapping symbol!).
                                if all_known_in_curr
                                    && only_import_edges
                                    && let Some(dep_parsed) = self.py_files.get(dep)
                                    && !dep_parsed.parse_fallback
                                {
                                    if let Some(ref caller_syms) = detail.caller_symbols
                                        && !caller_syms.is_empty()
                                    {
                                        next_affected = dep_parsed.affected_exports(caller_syms);
                                    } else if detail.caller_symbols.is_none() {
                                        let overlap: BTreeSet<String> =
                                            imported_names.intersection(aff).cloned().collect();
                                        if !overlap.is_empty()
                                            && overlap.iter().all(|n| {
                                                matches!(
                                                    dep_parsed.symbols.get(n).map(|d| d.kind),
                                                    Some(SymbolKind::ImportedName)
                                                )
                                            })
                                        {
                                            next_affected = dep_parsed.affected_exports(&overlap);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    let next_only_static = state.only_static_or_init
                        && (detail.kinds.contains(&EdgeKind::StaticImport)
                            || detail.kinds.contains(&EdgeKind::PackageInit));

                    let candidate = VisitState {
                        only_static_or_init: next_only_static,
                        affected_exports: next_affected,
                    };

                    let should_push = match visited.get_mut(dep) {
                        None => {
                            visited.insert(dep.clone(), candidate.clone());
                            true
                        }
                        Some(prev) => {
                            let mut broadened = false;
                            if !prev.only_static_or_init && candidate.only_static_or_init {
                                prev.only_static_or_init = true;
                                broadened = true;
                            }
                            match (&mut prev.affected_exports, &candidate.affected_exports) {
                                (None, _) => {}
                                (Some(_), None) => {
                                    prev.affected_exports = None;
                                    broadened = true;
                                }
                                (Some(prev_set), Some(cand_set)) => {
                                    let before_len = prev_set.len();
                                    prev_set.extend(cand_set.iter().cloned());
                                    if prev_set.len() > before_len {
                                        broadened = true;
                                    }
                                }
                            }
                            broadened
                        }
                    };

                    if should_push {
                        matched_any = true;
                        if self.tests_by_file.contains_key(dep) {
                            let via = if depth == 0 {
                                format!("direct_dep:{changed}")
                            } else {
                                format!("transitive_dep:{changed}->..->{curr}")
                            };
                            impacted_map.entry(dep.clone()).or_default().insert(via);
                        }
                        let merged = visited[dep].clone();
                        queue.push_back((dep.clone(), depth + 1, merged));
                    }
                }
            }

            // Directory watchers (covers NEW files added to watched dirs)
            for dir in ancestor_dirs(changed) {
                if let Some(watchers) = self.dir_watchers.get(&dir) {
                    for w in watchers {
                        matched_any = true;
                        if self.tests_by_file.contains_key(w) {
                            impacted_map
                                .entry(w.clone())
                                .or_default()
                                .insert(format!("dir_watch:{dir}<-{changed}"));
                        }
                    }
                }
            }

            if changed.contains("/migrations/") && changed.ends_with(".py") {
                matched_any = true;
                for (tf, deps) in &self.forward_deps {
                    if self.tests_by_file.contains_key(tf)
                        && deps
                            .values()
                            .any(|d| d.kinds.contains(&EdgeKind::DjangoDbMigrations))
                    {
                        impacted_map
                            .entry(tf.clone())
                            .or_default()
                            .insert(format!("django_migration:{changed}"));
                    }
                }
            }

            // Fail closed on unmapped files (unless a known non-impacting file or THIS file itself
            // had at least one symbol-pruned edge!)
            if !matched_any {
                let is_known_py = self.py_files.contains_key(changed);
                let is_safe = is_known_unreferenced_safe_file(changed)
                    || (is_known_py && file_symbol_pruned > 0);
                if !is_safe && self.config.unknown_path_policy == UnknownPathPolicy::Full {
                    effective_tier = ConfidenceTier::FullVerified;
                    full_fallback_reasons.push(format!(
                        "unmapped file under fail-closed unknown_path_policy=full: {changed}"
                    ));
                }
            }
        }

        // Check comment/format-only edits against source-introspecting tests (BOTH self and downstream!)
        for f in &comment_or_format_only {
            if self.tests_by_file.contains_key(f)
                && self
                    .py_files
                    .get(f)
                    .map(|p| p.uses_source_introspection)
                    .unwrap_or(false)
            {
                impacted_map
                    .entry(f.clone())
                    .or_default()
                    .insert(format!("raw_source_introspection_self:{f}"));
            }
            let mut visited = BTreeSet::new();
            let mut queue = VecDeque::new();
            visited.insert(f.clone());
            queue.push_back(f.clone());
            while let Some(curr) = queue.pop_front() {
                if let Some(dependents) = self.reverse_deps.get(&curr) {
                    for dep in dependents {
                        if visited.insert(dep.clone()) {
                            if let Some(parsed) = self.py_files.get(dep)
                                && self.tests_by_file.contains_key(dep)
                                && parsed.uses_source_introspection
                            {
                                impacted_map
                                    .entry(dep.clone())
                                    .or_default()
                                    .insert(format!("raw_source_introspection:{f}"));
                            }
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }
        }

        if effective_tier == ConfidenceTier::FullVerified {
            let reason = full_fallback_reasons
                .first()
                .cloned()
                .unwrap_or_else(|| "full_verified".to_string());
            for tf in self.tests_by_file.keys() {
                impacted_map
                    .entry(tf.clone())
                    .or_default()
                    .insert(format!("full_fallback:{reason}"));
            }
        }

        let mut impacted = Vec::new();
        let mut impacted_test_nodes = 0usize;
        let mut tainted_test_nodes = 0usize;

        for (tf, reasons) in impacted_map {
            let idxs = self
                .tests_by_file
                .get(&tf)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let has_tainted = idxs.iter().any(|&i| self.test_safety[i].is_tainted());
            tainted_test_nodes += idxs
                .iter()
                .filter(|&&i| self.test_safety[i].is_tainted())
                .count();
            impacted_test_nodes += idxs.len();
            impacted.push(ImpactedTestFile {
                test_file: tf,
                test_count: idxs.len(),
                reasons: reasons.into_iter().collect(),
                has_tainted_tests: has_tainted,
            });
        }

        let total_test_files = self.tests_by_file.len();
        let total_test_nodes = self.tests.len();
        let reduction_pct = if total_test_nodes > 0 {
            (1.0 - (impacted_test_nodes as f64 / total_test_nodes as f64)) * 100.0
        } else {
            0.0
        };

        ImpactResult {
            requested_tier,
            effective_tier,
            full_fallback_reasons,
            changed_files: normalized_changed,
            semantic_changed_files: semantic_changed,
            comment_or_format_only_files: comment_or_format_only,
            symbol_pruned_edges,
            total_test_files,
            impacted_test_files: impacted.len(),
            total_test_nodes,
            impacted_test_nodes,
            tainted_test_nodes,
            reduction_pct,
            impacted,
            eval_ms: start.elapsed().as_secs_f64() * 1000.0,
        }
    }
}

fn detect_source_roots(
    project: &PyrnConfig,
    py_files: &BTreeMap<String, ParsedPyFile>,
) -> Vec<String> {
    let mut roots = project.source_roots.clone();
    if py_files
        .keys()
        .any(|p| p.starts_with("src/") && p.ends_with("/__init__.py"))
        && !roots.iter().any(|r| r == "src")
    {
        roots.push("src".to_string());
    }
    if !roots.iter().any(String::is_empty) {
        roots.push(String::new());
    }
    roots
}

fn rel_path_to_module(rel: &str, source_roots: &[String]) -> Option<String> {
    if !rel.ends_with(".py") {
        return None;
    }
    let stripped = source_roots
        .iter()
        .find_map(|root| {
            if root.is_empty() {
                Some(rel)
            } else {
                rel.strip_prefix(&format!("{root}/"))
            }
        })
        .unwrap_or(rel);

    let without_ext = stripped.strip_suffix(".py")?;
    let mod_path = if let Some(pkg) = without_ext.strip_suffix("/__init__") {
        pkg
    } else if without_ext == "__init__" {
        return None;
    } else {
        without_ext
    };
    Some(mod_path.replace('/', "."))
}

/// Resolve an import into `(target_file, SymbolFilter)` pairs.
fn resolve_import_targets_with_symbols(
    imp: &crate::parser::ImportRef,
    current_mod: &str,
    is_pkg_init: bool,
    module_to_path: &BTreeMap<String, String>,
) -> Vec<(String, SymbolFilter)> {
    let mut map: BTreeMap<String, SymbolFilter> = BTreeMap::new();
    let mut record = |path: String, filter: SymbolFilter| {
        map.entry(path)
            .and_modify(|e| e.merge(filter.clone()))
            .or_insert(filter);
    };

    let base_mod = if imp.level > 0 {
        resolve_relative_module(current_mod, is_pkg_init, imp.level, imp.module.as_deref())
    } else {
        imp.module.clone()
    };

    if let Some(ref base) = base_mod {
        // `from base import name1, name2`
        let mut non_submodule_names = BTreeSet::new();
        for name in &imp.names {
            if name == "*" {
                if let Some(p) = module_to_path.get(base) {
                    record(p.clone(), SymbolFilter::All);
                }
                let prefix = format!("{base}.");
                for (m, p) in module_to_path {
                    if m.starts_with(&prefix) && !m[prefix.len()..].contains('.') {
                        record(p.clone(), SymbolFilter::All);
                    }
                }
            } else {
                let candidate = format!("{base}.{name}");
                if let Some(sub_path) = module_to_path.get(&candidate) {
                    // Importing a submodule (`from pkg import submod`): whole submodule imported
                    record(sub_path.clone(), SymbolFilter::All);
                } else {
                    non_submodule_names.insert(name.clone());
                }
            }
        }
        if let Some(p) = module_to_path.get(base) {
            if imp.is_whole_module || imp.is_wildcard {
                record(p.clone(), SymbolFilter::All);
            } else if !non_submodule_names.is_empty() {
                record(p.clone(), SymbolFilter::Named(non_submodule_names));
            } else {
                // Only submodules imported from package `base`: only executes `base/__init__.py` preamble.
                record(p.clone(), SymbolFilter::PreambleOnly);
            }
        }
    } else if imp.level == 0 {
        for name in &imp.names {
            if let Some(p) = module_to_path.get(name) {
                record(p.clone(), SymbolFilter::All);
            }
        }
    }

    map.into_iter().collect()
}

fn resolve_relative_module(
    current_mod: &str,
    is_pkg_init: bool,
    level: usize,
    sub_module: Option<&str>,
) -> Option<String> {
    if current_mod.is_empty() {
        return sub_module.map(ToOwned::to_owned);
    }
    let mut parts: Vec<&str> = current_mod.split('.').collect();
    let pops = if is_pkg_init {
        level.saturating_sub(1)
    } else {
        level
    };
    if pops > parts.len() {
        return None;
    }
    parts.truncate(parts.len() - pops);
    if let Some(sub) = sub_module
        && !sub.is_empty()
    {
        parts.extend(sub.split('.'));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("."))
    }
}

fn resolve_dotted_module(
    dotted: &str,
    module_to_path: &BTreeMap<String, String>,
) -> Option<String> {
    module_to_path.get(dotted).cloned()
}

fn resolve_dotted_module_or_prefix(
    dotted: &str,
    module_to_path: &BTreeMap<String, String>,
) -> Option<String> {
    let mut cur = dotted.trim();
    loop {
        if let Some(p) = module_to_path.get(cur) {
            return Some(p.clone());
        }
        {
            let idx = cur.rfind('.')?;
            cur = &cur[..idx]
        }
    }
}

fn package_init_ancestors(target: &str, py_files: &BTreeMap<String, ParsedPyFile>) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = parent_dir(target);
    while !cur.is_empty() {
        let candidate = format!("{cur}/__init__.py");
        if candidate != target && py_files.contains_key(&candidate) {
            out.push(candidate);
        }
        cur = parent_dir(&cur);
    }
    out
}

pub(crate) fn is_conftest_path(rel: &str) -> bool {
    rel == "conftest.py" || rel.ends_with("/conftest.py")
}

fn is_broad_code_root(dir: &str) -> bool {
    if matches!(
        dir,
        "src" | "tests" | "tests/unit" | "tests/integration" | "testproject" | "scripts"
    ) {
        return true;
    }
    if let Some(rest) = dir.strip_prefix("src/")
        && !rest.contains('/')
    {
        return true;
    }
    false
}

fn parent_dir(rel: &str) -> String {
    match rel.rfind('/') {
        Some(idx) => rel[..idx].to_string(),
        None => String::new(),
    }
}

fn ancestor_dirs(rel: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut cur = parent_dir(rel);
    while !cur.is_empty() {
        dirs.push(cur.clone());
        cur = parent_dir(&cur);
    }
    dirs
}

fn is_known_unreferenced_safe_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.ends_with(".md")
        || lower.ends_with(".txt")
        || lower.ends_with(".rst")
        || lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".svg")
        || lower == "license"
        || lower == ".gitignore"
        || lower == ".gitattributes"
        || lower == ".gitmessage"
        || lower == ".editorconfig"
}

fn compute_env_fingerprint(config: &GraphConfig, raw_hashes: &BTreeMap<String, String>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN_ENV_FP.as_bytes());
    hasher.update(b"\0");
    hasher.update(config.platform_tag.as_bytes());
    hasher.update(b"\0");
    hasher.update(config.python_version.as_bytes());
    hasher.update(b"\0");
    hasher.update(config.django_settings_module.as_bytes());
    hasher.update(b"\0");
    for key_file in [
        "uv.lock",
        "pyproject.toml",
        ".python-version",
        "pytest.ini",
        ".pyrn.toml",
        "pyrn.toml",
        "tests/conftest.py",
    ] {
        if let Some(h) = raw_hashes.get(key_file) {
            hasher.update(key_file.as_bytes());
            hasher.update(b"=");
            hasher.update(h.as_bytes());
            hasher.update(b"\n");
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn compute_project_fingerprint(
    source_roots: &[String],
    raw_hashes: &BTreeMap<String, String>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN_PROJECT_FP.as_bytes());
    hasher.update(b"\0");
    for r in source_roots {
        hasher.update(r.as_bytes());
        hasher.update(b";");
    }
    if let Some(h) = raw_hashes.get("pyproject.toml") {
        hasher.update(h.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}
