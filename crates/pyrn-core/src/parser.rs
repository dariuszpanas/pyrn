//! Parallel Python 3.12+ AST analyzer, dual BLAKE3 hasher (`raw_hash` + `ast_hash`),
//! and per-symbol AST dependency table for sound symbol-level TIA refinement.
//!
//! Pure over raw `&[u8]` slices (zero filesystem I/O, `#![forbid(unsafe_code)]`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rustpython_parser::Parse;
use rustpython_parser::ast::{
    self, ArgWithDefault, Arguments, Comprehension, Constant, ExceptHandler, Expr, Keyword,
    MatchCase, Operator, Pattern, Stmt,
};
use serde::{Deserialize, Serialize};

use crate::config::PyrnConfig;
use crate::tree::{DOMAIN_AST_BLOB, DOMAIN_AST_PREAMBLE, DOMAIN_AST_SYMBOL};

/// Parsed import statement (top-level or local).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRef {
    pub module: Option<String>,
    pub names: Vec<String>,
    pub level: usize,
    pub is_local: bool,
    /// True for `import a.b` (whole-module import) vs `from a.b import sym`.
    pub is_whole_module: bool,
    /// True for `from a.b import *`.
    pub is_wildcard: bool,
    /// True for `try: import mod; mod._attr = None except ImportError: pass` cache clearers.
    pub is_cache_reset_only: bool,
    /// True if inside a conftest fixture that is either non-autouse or DB-conditional.
    pub in_fixture_scope_filtered: bool,
    /// Enclosing top-level symbol name if this import is inside a `def`/`class` body.
    pub enclosing_symbol: Option<String>,
}

/// Top-level symbol kind inside a Python module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Class,
    Constant,
    StdlibImport,
    /// Non-stdlib `from mod import sym` binding (hashed in preamble too, so adding/changing
    /// the import itself stays conservative, while downstream edits to `sym` propagate cleanly).
    ImportedName,
}

/// Top-level symbol definition with its canonical AST BLAKE3 hash and intra-module identifier refs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolDef {
    pub name: String,
    pub kind: SymbolKind,
    pub ast_hash: String,
    /// All unqualified identifier (`Expr::Name`) references inside this symbol's definition.
    pub referenced_names: BTreeSet<String>,
}

/// Result of comparing two versions of a Python module at the symbol level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ChangedSymbols {
    /// Comment/whitespace/formatting-only edit: zero semantic AST change.
    None,
    /// Only these specific top-level symbols changed (module preamble unchanged, zero side effects).
    Some { names: BTreeSet<String> },
    /// Module preamble changed, file added/deleted, parse fallback, or non-symbol edit -> full file.
    All,
}

/// Discovered pytest fixture definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureDef {
    pub name: String,
    pub scope: String,
    pub autouse: bool,
    pub params: Vec<String>,
    pub is_db_conditional: bool,
    pub is_cache_reset_only: bool,
    pub local_import_targets: Vec<String>,
}

/// Discovered pytest test item (function or class method).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestNode {
    pub nodeid: String,
    pub file_path: String,
    pub symbol_name: String,
    pub class_name: Option<String>,
    pub markers: Vec<String>,
    pub fixture_params: Vec<String>,
    pub is_unittest_subclass: bool,
    pub uses_django_db: bool,
    pub uses_django_client_or_url: bool,
    pub uses_django_admin: bool,
    pub ast_hash: String,
    pub taint_reasons: Vec<String>,
}

/// Complete extracted AST metadata for one Python file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedPyFile {
    pub rel_path: String,
    pub raw_hash: String,
    pub ast_hash: String,
    /// Canonical AST hash of top-level executable statements outside pure `def`/`class`/pure-assign/stdlib-import.
    pub module_preamble_hash: String,
    /// Top-level symbols defined in this module (for sound symbol-level TIA & closure hashing).
    pub symbols: BTreeMap<String, SymbolDef>,
    /// Names referenced directly in module-level executable preamble statements.
    pub preamble_referenced_names: BTreeSet<String>,
    pub parse_fallback: bool,
    pub imports: Vec<ImportRef>,
    pub pytest_plugins: Vec<String>,
    pub fixtures: Vec<FixtureDef>,
    pub tests: Vec<TestNode>,
    pub string_module_refs: Vec<String>,
    pub django_call_commands: Vec<String>,
    pub asset_path_refs: Vec<String>,
    pub env_vars_read: Vec<String>,
    pub uses_django_db_module: bool,
    pub uses_migration_executor: bool,
    pub uses_django_admin: bool,
    pub uses_django_client_or_url: bool,
    pub uses_django_settings: bool,
    pub uses_django_tasks: bool,
    pub uses_source_introspection: bool,
    pub module_taint_reasons: Vec<String>,
}

impl ParsedPyFile {
    /// Compute the intra-file forward symbol closure needed to evaluate `seed_names`:
    /// all top-level symbols reachable by following `referenced_names` from `seed_names`.
    #[must_use]
    pub fn intra_symbol_deps(&self, seed_names: &BTreeSet<String>) -> BTreeSet<String> {
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        for name in seed_names {
            if self.symbols.contains_key(name) && visited.insert(name.clone()) {
                queue.push_back(name.clone());
            }
        }
        while let Some(curr) = queue.pop_front() {
            if let Some(def) = self.symbols.get(&curr) {
                for r in &def.referenced_names {
                    if self.symbols.contains_key(r) && visited.insert(r.clone()) {
                        queue.push_back(r.clone());
                    }
                }
            }
        }
        visited
    }

    /// Compute the intra-file reverse symbol closure of `changed_syms`:
    /// all top-level symbols in this file that directly or transitively reference any symbol in `changed_syms`.
    /// Returns `None` if any changed symbol is referenced by top-level preamble code (meaning the
    /// whole module's import-time behavior may have changed -> degrade conservatively to `All`).
    #[must_use]
    pub fn affected_exports(&self, changed_syms: &BTreeSet<String>) -> Option<BTreeSet<String>> {
        if self.parse_fallback {
            return None;
        }
        // Build reverse intra-file reference map: target_sym -> referencing_syms
        let mut rev: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (sym_name, def) in &self.symbols {
            for r in &def.referenced_names {
                rev.entry(r.as_str()).or_default().push(sym_name.as_str());
            }
        }

        let mut affected = BTreeSet::new();
        let mut queue = VecDeque::new();
        for s in changed_syms {
            if affected.insert(s.clone()) {
                queue.push_back(s.clone());
            }
        }
        while let Some(curr) = queue.pop_front() {
            // If module-level executable preamble directly references an affected symbol,
            // import-time behavior of the module could change for ALL importers -> fail closed.
            if self.preamble_referenced_names.contains(&curr) {
                return None;
            }
            if let Some(callers) = rev.get(curr.as_str()) {
                for &caller in callers {
                    if affected.insert(caller.to_string()) {
                        queue.push_back(caller.to_string());
                    }
                }
            }
        }
        Some(affected)
    }

    /// Compute a symbol-scoped BLAKE3 digest for a caller that imports ONLY `requested_names`.
    /// Falls back to full `self.ast_hash` if any requested name is unknown (e.g. re-export / `__getattr__`)
    /// or if this file had a parse fallback.
    #[must_use]
    pub fn symbol_scoped_digest(&self, requested_names: &BTreeSet<String>) -> String {
        if self.parse_fallback || requested_names.is_empty() {
            return self.ast_hash.clone();
        }
        // Fail closed to full AST hash if any requested symbol isn't a known top-level symbol
        // (e.g., dynamically constructed or re-exported via non-stdlib `from x import y`).
        if requested_names
            .iter()
            .any(|n| !self.symbols.contains_key(n))
        {
            return self.ast_hash.clone();
        }
        let closure = self.intra_symbol_deps(requested_names);
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"pyrn.sym-slice.v1\0");
        hasher.update(self.module_preamble_hash.as_bytes());
        hasher.update(b"\0");
        for sym in closure {
            hasher.update(sym.as_bytes());
            hasher.update(b"=");
            if let Some(def) = self.symbols.get(&sym) {
                hasher.update(def.ast_hash.as_bytes());
            }
            hasher.update(b"\n");
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// Compare two parsed snapshots of the same Python file and return the exact changed symbols.
#[must_use]
pub fn diff_python_symbols(before: &ParsedPyFile, after: &ParsedPyFile) -> ChangedSymbols {
    if before.parse_fallback || after.parse_fallback {
        return if before.raw_hash == after.raw_hash {
            ChangedSymbols::None
        } else {
            ChangedSymbols::All
        };
    }
    if before.ast_hash == after.ast_hash {
        return ChangedSymbols::None;
    }
    if before.module_preamble_hash != after.module_preamble_hash {
        return ChangedSymbols::All;
    }

    let mut changed = BTreeSet::new();
    for (name, after_def) in &after.symbols {
        match before.symbols.get(name) {
            Some(before_def) if before_def.ast_hash == after_def.ast_hash => {}
            _ => {
                changed.insert(name.clone());
            }
        }
    }
    for name in before.symbols.keys() {
        if !after.symbols.contains_key(name) {
            // Deleting or renaming a top-level symbol can break `from mod import name` at import
            // time even in modules that only re-export it (e.g. `__init__.py`) -> fail closed.
            return ChangedSymbols::All;
        }
    }

    if changed.is_empty() {
        // AST changed in a way not captured by symbol hashes (conservative fail-closed)
        ChangedSymbols::All
    } else {
        ChangedSymbols::Some { names: changed }
    }
}

const DB_FIXTURES: &[&str] = &[
    "db",
    "transactional_db",
    "django_db_reset_sequences",
    "django_db_serialized_rollback",
    "admin_client",
    "admin_user",
    "live_server",
];

const TAINTED_MARKERS: &[&str] = &[
    "real_ray",
    "live_cluster",
    "postgresql",
    "compiled_graph_opt_in",
];

const TAINTED_FIXTURES: &[&str] = &["ray_cluster", "live_ray_cluster", "live_server"];

/// Standard-library top-level modules guaranteed free of first-party import-time side effects.
/// Adding `import secrets` or `from pathlib import Path` binds a symbol without changing
/// the module preamble for unrelated functions in the same file.
const PURE_STDLIB_ROOTS: &[&str] = &[
    "__future__",
    "abc",
    "argparse",
    "array",
    "asyncio",
    "base64",
    "bisect",
    "collections",
    "concurrent",
    "contextlib",
    "copy",
    "csv",
    "dataclasses",
    "datetime",
    "decimal",
    "enum",
    "errno",
    "fnmatch",
    "fractions",
    "functools",
    "glob",
    "gzip",
    "hashlib",
    "heapq",
    "hmac",
    "html",
    "http",
    "importlib",
    "inspect",
    "io",
    "ipaddress",
    "itertools",
    "json",
    "logging",
    "math",
    "mimetypes",
    "numbers",
    "operator",
    "os",
    "pathlib",
    "platform",
    "pprint",
    "queue",
    "random",
    "re",
    "secrets",
    "shlex",
    "shutil",
    "signal",
    "socket",
    "sqlite3",
    "stat",
    "statistics",
    "string",
    "struct",
    "subprocess",
    "sys",
    "tarfile",
    "tempfile",
    "textwrap",
    "threading",
    "time",
    "tomllib",
    "traceback",
    "types",
    "typing",
    "unittest",
    "urllib",
    "uuid",
    "warnings",
    "weakref",
    "zipfile",
    "zlib",
];

/// Parse a Python file's bytes into structured TIA metadata, symbol table, and dual BLAKE3 hashes.
#[must_use]
pub fn parse_python_file(
    rel_path: &str,
    bytes: &[u8],
    raw_hash: &str,
    config: &PyrnConfig,
) -> ParsedPyFile {
    let source = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return fallback_parsed(rel_path, raw_hash, "", config),
    };

    let suite = match ast::Suite::parse(source, rel_path) {
        Ok(s) => s,
        Err(_) => return fallback_parsed(rel_path, raw_hash, source, config),
    };

    let ast_hash = compute_suite_ast_hash(&suite);
    let (module_preamble_hash, symbols, preamble_referenced_names) = extract_symbol_table(&suite);

    let mut ctx = ExtractCtx {
        rel_path: rel_path.to_string(),
        config,
        is_conftest: rel_path == "conftest.py" || rel_path.ends_with("/conftest.py"),
        imports: Vec::new(),
        pytest_plugins: Vec::new(),
        fixtures: Vec::new(),
        tests: Vec::new(),
        string_module_refs: BTreeSet::new(),
        django_call_commands: BTreeSet::new(),
        asset_path_refs: BTreeSet::new(),
        env_vars_read: BTreeSet::new(),
        module_markers: BTreeSet::new(),
        module_taint_reasons: BTreeSet::new(),
        uses_django_db_module: false,
        uses_migration_executor: false,
        uses_django_admin: false,
        uses_django_client_or_url: false,
        uses_django_settings: false,
        uses_django_tasks: false,
        uses_source_introspection: false,
    };

    for stmt in &suite {
        ctx.scan_top_level_metadata(stmt);
    }
    for stmt in &suite {
        ctx.visit_top_stmt(stmt);
    }

    let mod_uses_db = ctx.module_markers.contains("django_db") || ctx.uses_migration_executor;
    let mod_taints: Vec<String> = ctx.module_taint_reasons.iter().cloned().collect();
    for t in &mut ctx.tests {
        for m in &ctx.module_markers {
            if !t.markers.contains(m) {
                t.markers.push(m.clone());
            }
        }
        if mod_uses_db {
            t.uses_django_db = true;
        }
        for m in &t.markers {
            if TAINTED_MARKERS.contains(&m.as_str()) {
                let tag = format!("marker:{m}");
                if !t.taint_reasons.contains(&tag) {
                    t.taint_reasons.push(tag);
                }
            }
        }
        for fx in &t.fixture_params {
            if TAINTED_FIXTURES.contains(&fx.as_str()) {
                let tag = format!("fixture:{fx}");
                if !t.taint_reasons.contains(&tag) {
                    t.taint_reasons.push(tag);
                }
            }
        }
        for r in &mod_taints {
            if !t.taint_reasons.contains(r) {
                t.taint_reasons.push(r.clone());
            }
        }
        t.markers.sort();
        t.taint_reasons.sort();
    }

    ParsedPyFile {
        rel_path: rel_path.to_string(),
        raw_hash: raw_hash.to_string(),
        ast_hash,
        module_preamble_hash,
        symbols,
        preamble_referenced_names,
        parse_fallback: false,
        imports: ctx.imports,
        pytest_plugins: ctx.pytest_plugins,
        fixtures: ctx.fixtures,
        tests: ctx.tests,
        string_module_refs: ctx.string_module_refs.into_iter().collect(),
        django_call_commands: ctx.django_call_commands.into_iter().collect(),
        asset_path_refs: ctx.asset_path_refs.into_iter().collect(),
        env_vars_read: ctx.env_vars_read.into_iter().collect(),
        uses_django_db_module: ctx.uses_django_db_module || mod_uses_db,
        uses_migration_executor: ctx.uses_migration_executor,
        uses_django_admin: ctx.uses_django_admin,
        uses_django_client_or_url: ctx.uses_django_client_or_url,
        uses_django_settings: ctx.uses_django_settings,
        uses_django_tasks: ctx.uses_django_tasks,
        uses_source_introspection: ctx.uses_source_introspection,
        module_taint_reasons: mod_taints,
    }
}

/// Extract module-level symbol table (`def`, `async def`, `class`, pure top-level constant
/// assignments, and pure stdlib imports) vs executable module preamble hash.
fn extract_symbol_table(suite: &[Stmt]) -> (String, BTreeMap<String, SymbolDef>, BTreeSet<String>) {
    let mut symbols: BTreeMap<String, SymbolDef> = BTreeMap::new();
    let mut preamble_stmts: Vec<&Stmt> = Vec::new();
    let mut preamble_refs = BTreeSet::new();

    for (idx, stmt) in suite.iter().enumerate() {
        // Module docstring (`"""..."""`) is a pure expression constant at stmt 0 — keep in preamble
        match stmt {
            Stmt::FunctionDef(f) => {
                let mut refs = BTreeSet::new();
                collect_stmt_name_refs(stmt, &mut refs);
                refs.remove(f.name.as_str());
                insert_or_merge_symbol(
                    &mut symbols,
                    f.name.to_string(),
                    SymbolKind::Function,
                    compute_stmt_ast_hash(stmt),
                    refs,
                );
            }
            Stmt::AsyncFunctionDef(f) => {
                let mut refs = BTreeSet::new();
                collect_stmt_name_refs(stmt, &mut refs);
                refs.remove(f.name.as_str());
                insert_or_merge_symbol(
                    &mut symbols,
                    f.name.to_string(),
                    SymbolKind::Function,
                    compute_stmt_ast_hash(stmt),
                    refs,
                );
            }
            Stmt::ClassDef(c) => {
                let mut refs = BTreeSet::new();
                collect_stmt_name_refs(stmt, &mut refs);
                refs.remove(c.name.as_str());
                insert_or_merge_symbol(
                    &mut symbols,
                    c.name.to_string(),
                    SymbolKind::Class,
                    compute_stmt_ast_hash(stmt),
                    refs,
                );
            }
            Stmt::Assign(a)
                if a.targets.len() == 1
                    && matches!(a.targets.first(), Some(Expr::Name(_)))
                    && is_pure_constant_expr(&a.value) =>
            {
                if let Some(Expr::Name(n)) = a.targets.first() {
                    let mut refs = BTreeSet::new();
                    collect_expr_name_refs(&a.value, &mut refs);
                    refs.remove(n.id.as_str());
                    insert_or_merge_symbol(
                        &mut symbols,
                        n.id.to_string(),
                        SymbolKind::Constant,
                        compute_stmt_ast_hash(stmt),
                        refs,
                    );
                }
            }
            Stmt::AnnAssign(a)
                if matches!(a.target.as_ref(), Expr::Name(_))
                    && a.value
                        .as_deref()
                        .map(is_pure_constant_expr)
                        .unwrap_or(true) =>
            {
                if let Expr::Name(n) = a.target.as_ref() {
                    let mut refs = BTreeSet::new();
                    collect_expr_name_refs(&a.annotation, &mut refs);
                    if let Some(val) = &a.value {
                        collect_expr_name_refs(val, &mut refs);
                    }
                    refs.remove(n.id.as_str());
                    insert_or_merge_symbol(
                        &mut symbols,
                        n.id.to_string(),
                        SymbolKind::Constant,
                        compute_stmt_ast_hash(stmt),
                        refs,
                    );
                }
            }
            Stmt::Import(imp)
                if imp
                    .names
                    .iter()
                    .all(|alias| is_pure_stdlib_module(alias.name.as_str())) =>
            {
                let h = compute_stmt_ast_hash(stmt);
                for alias in &imp.names {
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            alias
                                .name
                                .as_str()
                                .split('.')
                                .next()
                                .unwrap_or(alias.name.as_str())
                                .to_string()
                        });
                    insert_or_merge_symbol(
                        &mut symbols,
                        bound,
                        SymbolKind::StdlibImport,
                        h.clone(),
                        BTreeSet::new(),
                    );
                }
            }
            Stmt::ImportFrom(imp)
                if imp.level.map(|l| l.to_usize()).unwrap_or(0) == 0
                    && imp
                        .module
                        .as_deref()
                        .map(is_pure_stdlib_module)
                        .unwrap_or(false)
                    && imp.names.iter().all(|a| a.name.as_str() != "*") =>
            {
                let h = compute_stmt_ast_hash(stmt);
                for alias in &imp.names {
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| alias.name.to_string());
                    insert_or_merge_symbol(
                        &mut symbols,
                        bound,
                        SymbolKind::StdlibImport,
                        h.clone(),
                        BTreeSet::new(),
                    );
                }
            }
            Stmt::ImportFrom(imp) if imp.names.iter().all(|a| a.name.as_str() != "*") => {
                // Non-stdlib `from x import y`: hash in preamble (adding/changing the import itself
                // is conservative `All`), AND register each bound name in `symbols` (NOT `preamble_refs`)
                // so symbol-only edits to `y` propagate strictly to functions/re-exporters that use `y`!
                let h = compute_stmt_ast_hash(stmt);
                for alias in &imp.names {
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| alias.name.to_string());
                    insert_or_merge_symbol(
                        &mut symbols,
                        bound,
                        SymbolKind::ImportedName,
                        h.clone(),
                        BTreeSet::new(),
                    );
                }
                preamble_stmts.push(stmt);
            }
            _ => {
                // Skip statements whose bodies never execute on module import from `preamble_refs`
                // (still hashed in `module_preamble_hash` so direct edits to them stay `All`!):
                // - module docstring at index 0
                // - `__all__ = [...]` string re-export lists
                // - `if __name__ == "__main__": ...` (never runs on `import`)
                // - `if TYPE_CHECKING:` / `if typing.TYPE_CHECKING:` (never runs at runtime)
                let is_docstring = idx == 0
                    && matches!(stmt, Stmt::Expr(e) if matches!(e.value.as_ref(), Expr::Constant(c) if matches!(c.value, Constant::Str(_))));
                let is_dunder_all = matches!(
                    stmt,
                    Stmt::Assign(a) if a.targets.len() == 1
                        && matches!(a.targets.first(), Some(Expr::Name(n)) if n.id.as_str() == "__all__")
                );
                if !is_docstring && !is_dunder_all && !is_non_import_time_if_guard(stmt) {
                    collect_stmt_name_refs(stmt, &mut preamble_refs);
                }
                preamble_stmts.push(stmt);
            }
        }
    }

    let mut pre_hasher = blake3::Hasher::new();
    pre_hasher.update(DOMAIN_AST_PREAMBLE.as_bytes());
    pre_hasher.update(b"\0");
    pre_hasher.update(&(preamble_stmts.len() as u32).to_le_bytes());
    for s in preamble_stmts {
        hash_stmt(&mut pre_hasher, s);
    }
    let preamble_hash = pre_hasher.finalize().to_hex().to_string();

    (preamble_hash, symbols, preamble_refs)
}

fn insert_or_merge_symbol(
    symbols: &mut BTreeMap<String, SymbolDef>,
    name: String,
    kind: SymbolKind,
    ast_hash: String,
    refs: BTreeSet<String>,
) {
    if let Some(existing) = symbols.get_mut(&name) {
        // Combine hashes deterministically when a symbol is redefined/overloaded
        let mut h = blake3::Hasher::new();
        h.update(existing.ast_hash.as_bytes());
        h.update(b"+");
        h.update(ast_hash.as_bytes());
        existing.ast_hash = h.finalize().to_hex().to_string();
        existing.referenced_names.extend(refs);
    } else {
        symbols.insert(
            name.clone(),
            SymbolDef {
                name,
                kind,
                ast_hash,
                referenced_names: refs,
            },
        );
    }
}

fn is_pure_stdlib_module(mod_name: &str) -> bool {
    let root = mod_name.split('.').next().unwrap_or(mod_name);
    PURE_STDLIB_ROOTS.contains(&root)
}

/// Conservative check for pure side-effect-free top-level constant expressions (literals, containers,
/// `frozenset(...)`, `tuple(...)`, `set(...)`, `re.compile(...)`, simple arithmetic/name refs).
fn is_pure_constant_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Constant(_) | Expr::Name(_) => true,
        Expr::UnaryOp(u) => is_pure_constant_expr(&u.operand),
        Expr::BinOp(b) => is_pure_constant_expr(&b.left) && is_pure_constant_expr(&b.right),
        Expr::Tuple(t) => t.elts.iter().all(is_pure_constant_expr),
        Expr::List(l) => l.elts.iter().all(is_pure_constant_expr),
        Expr::Set(s) => s.elts.iter().all(is_pure_constant_expr),
        Expr::Dict(d) => {
            d.keys
                .iter()
                .all(|k| k.as_ref().map(is_pure_constant_expr).unwrap_or(true))
                && d.values.iter().all(is_pure_constant_expr)
        }
        Expr::Attribute(a) => is_pure_constant_expr(&a.value),
        Expr::Call(c) => {
            let name = dotted_expr_name(&c.func).unwrap_or_default();
            matches!(
                name.as_str(),
                "frozenset" | "set" | "tuple" | "dict" | "list" | "re.compile" | "PurePosixPath"
            ) && c.args.iter().all(is_pure_constant_expr)
                && c.keywords.iter().all(|k| is_pure_constant_expr(&k.value))
        }
        _ => false,
    }
}

/// True for top-level `if` guards whose body NEVER executes when a module is imported:
/// - `if __name__ == "__main__": ...` (with empty `else`)
/// - `if TYPE_CHECKING:` / `if typing.TYPE_CHECKING:` (with empty `else`)
fn is_non_import_time_if_guard(stmt: &Stmt) -> bool {
    let Stmt::If(if_stmt) = stmt else {
        return false;
    };
    if !if_stmt.orelse.is_empty() {
        return false;
    }
    match if_stmt.test.as_ref() {
        Expr::Name(n) => n.id.as_str() == "TYPE_CHECKING",
        Expr::Attribute(a) => a.attr.as_str() == "TYPE_CHECKING",
        Expr::Compare(c) if c.ops.len() == 1 && c.comparators.len() == 1 => {
            if !matches!(c.ops[0], rustpython_parser::ast::CmpOp::Eq) {
                return false;
            }
            let is_dunder_name =
                |e: &Expr| matches!(e, Expr::Name(n) if n.id.as_str() == "__name__");
            let is_dunder_main = |e: &Expr| {
                matches!(
                    e,
                    Expr::Constant(k) if matches!(&k.value, Constant::Str(s) if s == "__main__")
                )
            };
            (is_dunder_name(&c.left) && is_dunder_main(&c.comparators[0]))
                || (is_dunder_main(&c.left) && is_dunder_name(&c.comparators[0]))
        }
        _ => false,
    }
}

fn collect_stmt_name_refs(stmt: &Stmt, out: &mut BTreeSet<String>) {
    match stmt {
        Stmt::FunctionDef(f) => {
            for d in &f.decorator_list {
                collect_expr_name_refs(d, out);
            }
            collect_args_name_refs(&f.args, out);
            if let Some(r) = &f.returns {
                collect_expr_name_refs(r, out);
            }
            for s in &f.body {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::AsyncFunctionDef(f) => {
            for d in &f.decorator_list {
                collect_expr_name_refs(d, out);
            }
            collect_args_name_refs(&f.args, out);
            if let Some(r) = &f.returns {
                collect_expr_name_refs(r, out);
            }
            for s in &f.body {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::ClassDef(c) => {
            for d in &c.decorator_list {
                collect_expr_name_refs(d, out);
            }
            for b in &c.bases {
                collect_expr_name_refs(b, out);
            }
            for kw in &c.keywords {
                collect_expr_name_refs(&kw.value, out);
            }
            for s in &c.body {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = &r.value {
                collect_expr_name_refs(v, out);
            }
        }
        Stmt::Delete(d) => {
            for t in &d.targets {
                collect_expr_name_refs(t, out);
            }
        }
        Stmt::Assign(a) => {
            for t in &a.targets {
                collect_expr_name_refs(t, out);
            }
            collect_expr_name_refs(&a.value, out);
        }
        Stmt::AugAssign(a) => {
            collect_expr_name_refs(&a.target, out);
            collect_expr_name_refs(&a.value, out);
        }
        Stmt::AnnAssign(a) => {
            collect_expr_name_refs(&a.target, out);
            collect_expr_name_refs(&a.annotation, out);
            if let Some(v) = &a.value {
                collect_expr_name_refs(v, out);
            }
        }
        Stmt::TypeAlias(ta) => {
            collect_expr_name_refs(&ta.value, out);
        }
        Stmt::For(f) => {
            collect_expr_name_refs(&f.iter, out);
            for s in &f.body {
                collect_stmt_name_refs(s, out);
            }
            for s in &f.orelse {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::AsyncFor(f) => {
            collect_expr_name_refs(&f.iter, out);
            for s in &f.body {
                collect_stmt_name_refs(s, out);
            }
            for s in &f.orelse {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::While(w) => {
            collect_expr_name_refs(&w.test, out);
            for s in &w.body {
                collect_stmt_name_refs(s, out);
            }
            for s in &w.orelse {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::If(i) => {
            collect_expr_name_refs(&i.test, out);
            for s in &i.body {
                collect_stmt_name_refs(s, out);
            }
            for s in &i.orelse {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::With(w) => {
            for it in &w.items {
                collect_expr_name_refs(&it.context_expr, out);
            }
            for s in &w.body {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::AsyncWith(w) => {
            for it in &w.items {
                collect_expr_name_refs(&it.context_expr, out);
            }
            for s in &w.body {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::Match(m) => {
            collect_expr_name_refs(&m.subject, out);
            for c in &m.cases {
                if let Some(g) = &c.guard {
                    collect_expr_name_refs(g, out);
                }
                for s in &c.body {
                    collect_stmt_name_refs(s, out);
                }
            }
        }
        Stmt::Raise(r) => {
            if let Some(e) = &r.exc {
                collect_expr_name_refs(e, out);
            }
            if let Some(c) = &r.cause {
                collect_expr_name_refs(c, out);
            }
        }
        Stmt::Try(t) => {
            for s in &t.body {
                collect_stmt_name_refs(s, out);
            }
            for h in &t.handlers {
                let ExceptHandler::ExceptHandler(eh) = h;
                if let Some(tp) = &eh.type_ {
                    collect_expr_name_refs(tp, out);
                }
                for s in &eh.body {
                    collect_stmt_name_refs(s, out);
                }
            }
            for s in &t.orelse {
                collect_stmt_name_refs(s, out);
            }
            for s in &t.finalbody {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::TryStar(t) => {
            for s in &t.body {
                collect_stmt_name_refs(s, out);
            }
            for h in &t.handlers {
                let ExceptHandler::ExceptHandler(eh) = h;
                if let Some(tp) = &eh.type_ {
                    collect_expr_name_refs(tp, out);
                }
                for s in &eh.body {
                    collect_stmt_name_refs(s, out);
                }
            }
            for s in &t.orelse {
                collect_stmt_name_refs(s, out);
            }
            for s in &t.finalbody {
                collect_stmt_name_refs(s, out);
            }
        }
        Stmt::Assert(a) => {
            collect_expr_name_refs(&a.test, out);
            if let Some(m) = &a.msg {
                collect_expr_name_refs(m, out);
            }
        }
        Stmt::Expr(e) => collect_expr_name_refs(&e.value, out),
        Stmt::Import(_)
        | Stmt::ImportFrom(_)
        | Stmt::Global(_)
        | Stmt::Nonlocal(_)
        | Stmt::Pass(_)
        | Stmt::Break(_)
        | Stmt::Continue(_) => {}
    }
}

fn collect_args_name_refs(args: &Arguments, out: &mut BTreeSet<String>) {
    for a in args
        .posonlyargs
        .iter()
        .chain(args.args.iter())
        .chain(args.kwonlyargs.iter())
    {
        if let Some(ann) = &a.def.annotation {
            collect_expr_name_refs(ann, out);
        }
        if let Some(def) = &a.default {
            collect_expr_name_refs(def, out);
        }
    }
    if let Some(va) = &args.vararg
        && let Some(ann) = &va.annotation
    {
        collect_expr_name_refs(ann, out);
    }
    if let Some(ka) = &args.kwarg
        && let Some(ann) = &ka.annotation
    {
        collect_expr_name_refs(ann, out);
    }
}

fn collect_expr_name_refs(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Name(n) => {
            out.insert(n.id.to_string());
        }
        Expr::BoolOp(b) => {
            for v in &b.values {
                collect_expr_name_refs(v, out);
            }
        }
        Expr::NamedExpr(n) => {
            collect_expr_name_refs(&n.target, out);
            collect_expr_name_refs(&n.value, out);
        }
        Expr::BinOp(b) => {
            collect_expr_name_refs(&b.left, out);
            collect_expr_name_refs(&b.right, out);
        }
        Expr::UnaryOp(u) => collect_expr_name_refs(&u.operand, out),
        Expr::Lambda(l) => {
            collect_args_name_refs(&l.args, out);
            collect_expr_name_refs(&l.body, out);
        }
        Expr::IfExp(i) => {
            collect_expr_name_refs(&i.test, out);
            collect_expr_name_refs(&i.body, out);
            collect_expr_name_refs(&i.orelse, out);
        }
        Expr::Dict(d) => {
            for k in d.keys.iter().flatten() {
                collect_expr_name_refs(k, out);
            }
            for v in &d.values {
                collect_expr_name_refs(v, out);
            }
        }
        Expr::Set(s) => {
            for e in &s.elts {
                collect_expr_name_refs(e, out);
            }
        }
        Expr::List(l) => {
            for e in &l.elts {
                collect_expr_name_refs(e, out);
            }
        }
        Expr::Tuple(t) => {
            for e in &t.elts {
                collect_expr_name_refs(e, out);
            }
        }
        Expr::ListComp(c) => {
            collect_expr_name_refs(&c.elt, out);
            for g in &c.generators {
                collect_comp_name_refs(g, out);
            }
        }
        Expr::SetComp(c) => {
            collect_expr_name_refs(&c.elt, out);
            for g in &c.generators {
                collect_comp_name_refs(g, out);
            }
        }
        Expr::DictComp(c) => {
            collect_expr_name_refs(&c.key, out);
            collect_expr_name_refs(&c.value, out);
            for g in &c.generators {
                collect_comp_name_refs(g, out);
            }
        }
        Expr::GeneratorExp(c) => {
            collect_expr_name_refs(&c.elt, out);
            for g in &c.generators {
                collect_comp_name_refs(g, out);
            }
        }
        Expr::Await(a) => collect_expr_name_refs(&a.value, out),
        Expr::Yield(y) => {
            if let Some(v) = &y.value {
                collect_expr_name_refs(v, out);
            }
        }
        Expr::YieldFrom(y) => collect_expr_name_refs(&y.value, out),
        Expr::Compare(c) => {
            collect_expr_name_refs(&c.left, out);
            for comp in &c.comparators {
                collect_expr_name_refs(comp, out);
            }
        }
        Expr::Call(c) => {
            collect_expr_name_refs(&c.func, out);
            for a in &c.args {
                collect_expr_name_refs(a, out);
            }
            for kw in &c.keywords {
                collect_expr_name_refs(&kw.value, out);
            }
        }
        Expr::FormattedValue(fv) => {
            collect_expr_name_refs(&fv.value, out);
            if let Some(fs) = &fv.format_spec {
                collect_expr_name_refs(fs, out);
            }
        }
        Expr::JoinedStr(js) => {
            for v in &js.values {
                collect_expr_name_refs(v, out);
            }
        }
        Expr::Attribute(a) => collect_expr_name_refs(&a.value, out),
        Expr::Subscript(s) => {
            collect_expr_name_refs(&s.value, out);
            collect_expr_name_refs(&s.slice, out);
        }
        Expr::Starred(s) => collect_expr_name_refs(&s.value, out),
        Expr::Slice(s) => {
            if let Some(l) = &s.lower {
                collect_expr_name_refs(l, out);
            }
            if let Some(u) = &s.upper {
                collect_expr_name_refs(u, out);
            }
            if let Some(st) = &s.step {
                collect_expr_name_refs(st, out);
            }
        }
        Expr::Constant(_) => {}
    }
}

fn collect_comp_name_refs(g: &Comprehension, out: &mut BTreeSet<String>) {
    collect_expr_name_refs(&g.iter, out);
    for cond in &g.ifs {
        collect_expr_name_refs(cond, out);
    }
}

/// Fallback extractor when a file fails to parse: fail-closed with raw hash and line-based scraping.
fn fallback_parsed(
    rel_path: &str,
    raw_hash: &str,
    source: &str,
    config: &PyrnConfig,
) -> ParsedPyFile {
    let mut imports = Vec::new();
    let mut tests = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("import ") {
            let names: Vec<String> = rest
                .split(',')
                .filter_map(|p| p.split_whitespace().next())
                .map(ToOwned::to_owned)
                .collect();
            imports.push(ImportRef {
                module: None,
                names,
                level: 0,
                is_local: line.starts_with(' ') || line.starts_with('\t'),
                is_whole_module: true,
                is_wildcard: false,
                is_cache_reset_only: false,
                in_fixture_scope_filtered: false,
                enclosing_symbol: None,
            });
        } else if let Some(rest) = trimmed.strip_prefix("from ") {
            if let Some((mod_part, imp_part)) = rest.split_once(" import ") {
                let m = mod_part.trim();
                let level = m.chars().take_while(|&c| c == '.').count();
                let mod_name = m[level..].trim();
                let names: Vec<String> = imp_part
                    .split(',')
                    .filter_map(|p| {
                        p.trim()
                            .trim_matches(|c| c == '(' || c == ')')
                            .split_whitespace()
                            .next()
                    })
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
                let is_wildcard = names.iter().any(|n| n == "*");
                imports.push(ImportRef {
                    module: if mod_name.is_empty() {
                        None
                    } else {
                        Some(mod_name.to_string())
                    },
                    names,
                    level,
                    is_local: line.starts_with(' ') || line.starts_with('\t'),
                    is_whole_module: false,
                    is_wildcard,
                    is_cache_reset_only: false,
                    in_fixture_scope_filtered: false,
                    enclosing_symbol: None,
                });
            }
        } else if let Some(rest) = trimmed.strip_prefix("def ")
            && let Some(fn_name) = rest.split('(').next().map(str::trim)
            && config.matches_test_function(fn_name)
        {
            tests.push(TestNode {
                nodeid: format!("{rel_path}::{fn_name}"),
                file_path: rel_path.to_string(),
                symbol_name: fn_name.to_string(),
                class_name: None,
                markers: Vec::new(),
                fixture_params: Vec::new(),
                is_unittest_subclass: false,
                uses_django_db: false,
                uses_django_client_or_url: false,
                uses_django_admin: false,
                ast_hash: raw_hash.to_string(),
                taint_reasons: vec!["parse_fallback".to_string()],
            });
        }
    }

    ParsedPyFile {
        rel_path: rel_path.to_string(),
        raw_hash: raw_hash.to_string(),
        ast_hash: raw_hash.to_string(),
        module_preamble_hash: raw_hash.to_string(),
        symbols: BTreeMap::new(),
        preamble_referenced_names: BTreeSet::new(),
        parse_fallback: true,
        imports,
        pytest_plugins: Vec::new(),
        fixtures: Vec::new(),
        tests,
        string_module_refs: Vec::new(),
        django_call_commands: Vec::new(),
        asset_path_refs: Vec::new(),
        env_vars_read: Vec::new(),
        uses_django_db_module: source.contains("django_db") || source.contains("MigrationExecutor"),
        uses_migration_executor: source.contains("MigrationExecutor"),
        uses_django_admin: source.contains("django.contrib.admin"),
        uses_django_client_or_url: source.contains("django.urls")
            || source.contains("TemplateResponse"),
        uses_django_settings: source.contains("override_settings")
            || source.contains("django.conf"),
        uses_django_tasks: source.contains("django.tasks") || source.contains("task_backends"),
        uses_source_introspection: true,
        module_taint_reasons: vec!["parse_fallback".to_string()],
    }
}

struct ExtractCtx<'a> {
    rel_path: String,
    config: &'a PyrnConfig,
    is_conftest: bool,
    imports: Vec<ImportRef>,
    pytest_plugins: Vec<String>,
    fixtures: Vec<FixtureDef>,
    tests: Vec<TestNode>,
    string_module_refs: BTreeSet<String>,
    django_call_commands: BTreeSet<String>,
    asset_path_refs: BTreeSet<String>,
    env_vars_read: BTreeSet<String>,
    module_markers: BTreeSet<String>,
    module_taint_reasons: BTreeSet<String>,
    uses_django_db_module: bool,
    uses_migration_executor: bool,
    uses_django_admin: bool,
    uses_django_client_or_url: bool,
    uses_django_settings: bool,
    uses_django_tasks: bool,
    uses_source_introspection: bool,
}

impl ExtractCtx<'_> {
    fn scan_top_level_metadata(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign(a) => {
                for target in &a.targets {
                    if let Expr::Name(n) = target {
                        match n.id.as_str() {
                            "pytest_plugins" => {
                                collect_string_literals(&a.value, &mut self.pytest_plugins);
                            }
                            "pytestmark" => {
                                let mut marks = Vec::new();
                                extract_markers_from_expr(&a.value, &mut marks);
                                self.module_markers.extend(marks);
                            }
                            _ => {}
                        }
                    }
                }
            }
            Stmt::AnnAssign(a) => {
                if let Expr::Name(n) = a.target.as_ref()
                    && n.id.as_str() == "pytestmark"
                    && let Some(val) = &a.value
                {
                    let mut marks = Vec::new();
                    extract_markers_from_expr(val, &mut marks);
                    self.module_markers.extend(marks);
                }
            }
            _ => {}
        }
    }

    fn visit_top_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Import(imp) => {
                let names: Vec<String> = imp.names.iter().map(|a| a.name.to_string()).collect();
                self.check_import_signals(None, &names);
                self.imports.push(ImportRef {
                    module: None,
                    names,
                    level: 0,
                    is_local: false,
                    is_whole_module: true,
                    is_wildcard: false,
                    is_cache_reset_only: false,
                    in_fixture_scope_filtered: false,
                    enclosing_symbol: None,
                });
            }
            Stmt::ImportFrom(imp) => {
                let module = imp.module.as_ref().map(|m| m.to_string());
                let names: Vec<String> = imp.names.iter().map(|a| a.name.to_string()).collect();
                let is_wildcard = names.iter().any(|n| n == "*");
                let level = imp.level.map(|i| i.to_usize()).unwrap_or(0);
                self.check_import_signals(module.as_deref(), &names);
                self.imports.push(ImportRef {
                    module,
                    names,
                    level,
                    is_local: false,
                    is_whole_module: false,
                    is_wildcard,
                    is_cache_reset_only: false,
                    in_fixture_scope_filtered: false,
                    enclosing_symbol: None,
                });
            }
            Stmt::FunctionDef(f) => {
                self.visit_function(
                    &f.name,
                    &f.args,
                    &f.decorator_list,
                    &f.body,
                    stmt,
                    None,
                    false,
                    &[],
                );
            }
            Stmt::AsyncFunctionDef(f) => {
                self.visit_function(
                    &f.name,
                    &f.args,
                    &f.decorator_list,
                    &f.body,
                    stmt,
                    None,
                    false,
                    &[],
                );
            }
            Stmt::ClassDef(c) => {
                self.visit_class(c);
            }
            _ => {
                self.visit_stmt_deep(stmt, false, false, false, None, &mut Vec::new());
            }
        }
    }

    fn visit_class(&mut self, c: &ast::StmtClassDef) {
        let mut class_markers = Vec::new();
        for dec in &c.decorator_list {
            extract_markers_from_expr(dec, &mut class_markers);
            self.visit_expr(dec, &mut Vec::new());
        }

        let mut is_unittest_subclass = false;
        let mut is_django_testcase = false;
        for base in &c.bases {
            self.visit_expr(base, &mut Vec::new());
            let base_name = dotted_expr_name(base).unwrap_or_default();
            if base_name.ends_with("TestCase")
                || base_name.ends_with("TransactionTestCase")
                || base_name.ends_with("SimpleTestCase")
                || base_name.ends_with("LiveServerTestCase")
            {
                is_unittest_subclass = true;
                if base_name != "SimpleTestCase" && !base_name.starts_with("unittest.") {
                    is_django_testcase = true;
                }
            }
        }
        if is_django_testcase {
            self.uses_django_db_module = true;
        }

        let is_test_class = self.config.matches_test_class(c.name.as_str()) || is_unittest_subclass;

        for body_stmt in &c.body {
            match body_stmt {
                Stmt::FunctionDef(m) if is_test_class => {
                    self.visit_function(
                        &m.name,
                        &m.args,
                        &m.decorator_list,
                        &m.body,
                        body_stmt,
                        Some(c.name.as_str()),
                        is_unittest_subclass,
                        &class_markers,
                    );
                }
                Stmt::AsyncFunctionDef(m) if is_test_class => {
                    self.visit_function(
                        &m.name,
                        &m.args,
                        &m.decorator_list,
                        &m.body,
                        body_stmt,
                        Some(c.name.as_str()),
                        is_unittest_subclass,
                        &class_markers,
                    );
                }
                _ => {
                    self.visit_stmt_deep(
                        body_stmt,
                        true,
                        false,
                        false,
                        Some(c.name.as_str()),
                        &mut Vec::new(),
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_function(
        &mut self,
        name: &ast::Identifier,
        args: &Arguments,
        decorators: &[Expr],
        body: &[Stmt],
        full_stmt: &Stmt,
        class_name: Option<&str>,
        is_unittest_subclass: bool,
        inherited_markers: &[String],
    ) {
        let mut markers: Vec<String> = inherited_markers.to_vec();
        let mut fixture_meta: Option<(String, bool)> = None;
        let mut dec_taints = Vec::new();

        for dec in decorators {
            extract_markers_from_expr(dec, &mut markers);
            if let Some(meta) = extract_fixture_decorator(dec) {
                fixture_meta = Some(meta);
            }
            self.visit_expr(dec, &mut dec_taints);
        }

        let params = extract_param_names(args);
        let is_cache_reset_only = self.is_conftest && is_cache_reset_fixture_body(body);
        let is_explicit_pure_guard = self.is_conftest
            && self
                .config
                .pure_autouse_guards
                .iter()
                .any(|g| g == name.as_str());
        let is_db_conditional = self.is_conftest
            && fixture_meta.as_ref().map(|(_, a)| *a).unwrap_or(false)
            && (is_db_conditional_fixture_body(body) || is_explicit_pure_guard);

        let filter_conftest_local_imports = self.is_conftest
            && fixture_meta
                .as_ref()
                .map(|(_, autouse)| !autouse || is_db_conditional || is_cache_reset_only)
                .unwrap_or(false);

        let enclosing_sym = class_name.unwrap_or(name.as_str());
        let mut fn_taints = dec_taints;
        let prev_import_len = self.imports.len();
        for s in body {
            self.visit_stmt_deep(
                s,
                true,
                is_cache_reset_only,
                filter_conftest_local_imports,
                Some(enclosing_sym),
                &mut fn_taints,
            );
        }

        let mut local_import_targets = Vec::new();
        for imp in &self.imports[prev_import_len..] {
            if let Some(ref m) = imp.module {
                local_import_targets.push(m.clone());
            } else {
                local_import_targets.extend(imp.names.iter().cloned());
            }
        }

        if let Some((scope, autouse)) = fixture_meta {
            self.fixtures.push(FixtureDef {
                name: name.to_string(),
                scope,
                autouse,
                params,
                is_db_conditional,
                is_cache_reset_only,
                local_import_targets,
            });
            return;
        }

        let is_test_name = self.config.matches_test_function(name.as_str());
        let is_in_test_file = self.config.matches_test_filename(&self.rel_path)
            || self.config.testpaths.iter().any(|tp| {
                self.rel_path
                    .starts_with(&format!("{}/", tp.trim_end_matches('/')))
            });

        if is_test_name && (is_in_test_file || class_name.is_some()) {
            let nodeid = match class_name {
                Some(cls) => format!("{}::{}::{}", self.rel_path, cls, name),
                None => format!("{}::{}", self.rel_path, name),
            };

            let uses_django_db = is_unittest_subclass
                || markers.iter().any(|m| m == "django_db")
                || params.iter().any(|p| DB_FIXTURES.contains(&p.as_str()));
            let uses_django_client_or_url = params
                .iter()
                .any(|p| matches!(p.as_str(), "client" | "admin_client" | "rf" | "live_server"))
                || self.uses_django_client_or_url;
            let uses_django_admin = params
                .iter()
                .any(|p| matches!(p.as_str(), "admin_client" | "admin_user"))
                || self.uses_django_admin;

            if uses_django_db {
                self.uses_django_db_module = true;
            }

            let sym_hash = compute_stmt_ast_hash(full_stmt);

            self.tests.push(TestNode {
                nodeid,
                file_path: self.rel_path.clone(),
                symbol_name: name.to_string(),
                class_name: class_name.map(ToOwned::to_owned),
                markers,
                fixture_params: params,
                is_unittest_subclass,
                uses_django_db,
                uses_django_client_or_url,
                uses_django_admin,
                ast_hash: sym_hash,
                taint_reasons: fn_taints,
            });
        }
    }

    fn visit_stmt_deep(
        &mut self,
        stmt: &Stmt,
        is_local: bool,
        is_cache_reset_only: bool,
        in_fixture_scope_filtered: bool,
        enclosing_symbol: Option<&str>,
        taints: &mut Vec<String>,
    ) {
        match stmt {
            Stmt::Import(imp) => {
                let names: Vec<String> = imp.names.iter().map(|a| a.name.to_string()).collect();
                self.check_import_signals(None, &names);
                self.imports.push(ImportRef {
                    module: None,
                    names,
                    level: 0,
                    is_local,
                    is_whole_module: true,
                    is_wildcard: false,
                    is_cache_reset_only,
                    in_fixture_scope_filtered,
                    enclosing_symbol: enclosing_symbol.map(ToOwned::to_owned),
                });
            }
            Stmt::ImportFrom(imp) => {
                let module = imp.module.as_ref().map(|m| m.to_string());
                let names: Vec<String> = imp.names.iter().map(|a| a.name.to_string()).collect();
                let is_wildcard = names.iter().any(|n| n == "*");
                let level = imp.level.map(|i| i.to_usize()).unwrap_or(0);
                self.check_import_signals(module.as_deref(), &names);
                self.imports.push(ImportRef {
                    module,
                    names,
                    level,
                    is_local,
                    is_whole_module: false,
                    is_wildcard,
                    is_cache_reset_only,
                    in_fixture_scope_filtered,
                    enclosing_symbol: enclosing_symbol.map(ToOwned::to_owned),
                });
            }
            Stmt::FunctionDef(f) => {
                for d in &f.decorator_list {
                    self.visit_expr(d, taints);
                }
                for s in &f.body {
                    self.visit_stmt_deep(
                        s,
                        true,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::AsyncFunctionDef(f) => {
                for d in &f.decorator_list {
                    self.visit_expr(d, taints);
                }
                for s in &f.body {
                    self.visit_stmt_deep(
                        s,
                        true,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::ClassDef(c) => {
                for d in &c.decorator_list {
                    self.visit_expr(d, taints);
                }
                for b in &c.bases {
                    self.visit_expr(b, taints);
                }
                for s in &c.body {
                    self.visit_stmt_deep(
                        s,
                        true,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::Return(r) => {
                if let Some(v) = &r.value {
                    self.visit_expr(v, taints);
                }
            }
            Stmt::Delete(d) => {
                for t in &d.targets {
                    self.visit_expr(t, taints);
                }
            }
            Stmt::Assign(a) => {
                for t in &a.targets {
                    self.visit_expr(t, taints);
                }
                self.visit_expr(&a.value, taints);
            }
            Stmt::AugAssign(a) => {
                self.visit_expr(&a.target, taints);
                self.visit_expr(&a.value, taints);
            }
            Stmt::AnnAssign(a) => {
                self.visit_expr(&a.target, taints);
                if let Some(v) = &a.value {
                    self.visit_expr(v, taints);
                }
            }
            Stmt::For(f) => {
                self.visit_expr(&f.iter, taints);
                for s in &f.body {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
                for s in &f.orelse {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::AsyncFor(f) => {
                self.visit_expr(&f.iter, taints);
                for s in &f.body {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
                for s in &f.orelse {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::While(w) => {
                self.visit_expr(&w.test, taints);
                for s in &w.body {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
                for s in &w.orelse {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::If(i) => {
                self.visit_expr(&i.test, taints);
                for s in &i.body {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
                for s in &i.orelse {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::With(w) => {
                for item in &w.items {
                    self.visit_expr(&item.context_expr, taints);
                }
                for s in &w.body {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::AsyncWith(w) => {
                for item in &w.items {
                    self.visit_expr(&item.context_expr, taints);
                }
                for s in &w.body {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::Try(t) => {
                for s in &t.body {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
                for h in &t.handlers {
                    let ExceptHandler::ExceptHandler(eh) = h;
                    for s in &eh.body {
                        self.visit_stmt_deep(
                            s,
                            is_local,
                            is_cache_reset_only,
                            in_fixture_scope_filtered,
                            enclosing_symbol,
                            taints,
                        );
                    }
                }
                for s in &t.orelse {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
                for s in &t.finalbody {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::TryStar(t) => {
                for s in &t.body {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
                for h in &t.handlers {
                    let ExceptHandler::ExceptHandler(eh) = h;
                    for s in &eh.body {
                        self.visit_stmt_deep(
                            s,
                            is_local,
                            is_cache_reset_only,
                            in_fixture_scope_filtered,
                            enclosing_symbol,
                            taints,
                        );
                    }
                }
                for s in &t.orelse {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
                for s in &t.finalbody {
                    self.visit_stmt_deep(
                        s,
                        is_local,
                        is_cache_reset_only,
                        in_fixture_scope_filtered,
                        enclosing_symbol,
                        taints,
                    );
                }
            }
            Stmt::Match(m) => {
                self.visit_expr(&m.subject, taints);
                for case in &m.cases {
                    if let Some(g) = &case.guard {
                        self.visit_expr(g, taints);
                    }
                    for s in &case.body {
                        self.visit_stmt_deep(
                            s,
                            is_local,
                            is_cache_reset_only,
                            in_fixture_scope_filtered,
                            enclosing_symbol,
                            taints,
                        );
                    }
                }
            }
            Stmt::Raise(r) => {
                if let Some(e) = &r.exc {
                    self.visit_expr(e, taints);
                }
                if let Some(c) = &r.cause {
                    self.visit_expr(c, taints);
                }
            }
            Stmt::Assert(a) => {
                self.visit_expr(&a.test, taints);
                if let Some(m) = &a.msg {
                    self.visit_expr(m, taints);
                }
            }
            Stmt::Expr(e) => self.visit_expr(&e.value, taints),
            Stmt::TypeAlias(ta) => self.visit_expr(&ta.value, taints),
            Stmt::Global(_)
            | Stmt::Nonlocal(_)
            | Stmt::Pass(_)
            | Stmt::Break(_)
            | Stmt::Continue(_) => {}
        }
    }

    fn visit_expr(&mut self, expr: &Expr, taints: &mut Vec<String>) {
        match expr {
            Expr::Call(call) => {
                let callee = dotted_expr_name(&call.func).unwrap_or_default();

                if (callee == "patch"
                    || callee.ends_with(".patch")
                    || callee == "import_module"
                    || callee.ends_with(".import_module")
                    || callee == "import_string"
                    || callee.ends_with(".import_string")
                    || callee.ends_with(".setattr"))
                    && let Some(Expr::Constant(c)) = call.args.first()
                    && let Constant::Str(ref s) = c.value
                    && looks_like_dotted_module(s)
                {
                    self.string_module_refs.insert(s.clone());
                }

                if (callee == "call_command" || callee.ends_with(".call_command"))
                    && let Some(Expr::Constant(c)) = call.args.first()
                    && let Constant::Str(ref s) = c.value
                {
                    self.django_call_commands.insert(s.clone());
                }

                if callee == "MigrationExecutor" || callee.ends_with(".MigrationExecutor") {
                    self.uses_migration_executor = true;
                    self.uses_django_db_module = true;
                }
                if callee == "override_settings"
                    || callee.ends_with(".override_settings")
                    || callee == "modify_settings"
                    || callee.ends_with(".modify_settings")
                {
                    self.uses_django_settings = true;
                }
                if callee == "reverse" || callee.ends_with(".reverse") {
                    self.uses_django_client_or_url = true;
                }

                if (callee == "os.getenv"
                    || callee == "getenv"
                    || callee == "os.environ.get"
                    || callee == "environ.get")
                    && let Some(Expr::Constant(c)) = call.args.first()
                    && let Constant::Str(ref k) = c.value
                {
                    self.env_vars_read.insert(k.clone());
                }

                if callee == "inspect.getsource"
                    || callee == "inspect.getsourcelines"
                    || callee == "getsource"
                    || callee == "getsourcelines"
                    || callee == "ast.parse"
                    || callee == "tokenize.generate_tokens"
                {
                    self.uses_source_introspection = true;
                }

                if callee == "subprocess.run"
                    || callee == "subprocess.Popen"
                    || callee == "subprocess.check_output"
                    || callee == "subprocess.check_call"
                {
                    let tag = format!("call:{callee}");
                    if !taints.contains(&tag) {
                        taints.push(tag);
                    }
                }
                if callee == "ray.init" || callee == "init_local_ray" {
                    let tag = format!("call:{callee}");
                    if !taints.contains(&tag) {
                        taints.push(tag);
                    }
                }
                if callee == "socket.create_connection"
                    || callee == "urllib.request.urlopen"
                    || callee == "requests.get"
                    || callee == "requests.post"
                {
                    let tag = format!("network:{callee}");
                    if !taints.contains(&tag) {
                        taints.push(tag);
                    }
                }

                let is_settings_or_conftest = self.is_conftest
                    || self.rel_path.ends_with("/settings.py")
                    || self.rel_path.ends_with("_settings.py")
                    || self.rel_path.ends_with("/__init__.py");
                for kw in &call.keywords {
                    if let Some(ref arg_name) = kw.arg {
                        let is_lazy_cfg = matches!(arg_name.as_str(), "ROOT_URLCONF" | "BACKEND");
                        if ((!is_settings_or_conftest && is_lazy_cfg)
                            || matches!(
                                arg_name.as_str(),
                                "callable_path" | "DJANGO_SETTINGS_MODULE"
                            ))
                            && let Expr::Constant(c) = &kw.value
                            && let Constant::Str(ref s) = c.value
                            && looks_like_dotted_module(s)
                        {
                            self.string_module_refs.insert(s.clone());
                        }
                    }
                }

                self.visit_expr(&call.func, taints);
                for arg in &call.args {
                    self.visit_expr(arg, taints);
                }
                for kw in &call.keywords {
                    self.visit_expr(&kw.value, taints);
                }
            }
            Expr::BinOp(binop) => {
                if matches!(binop.op, Operator::Div)
                    && let Some(joined) = extract_path_div_chain(expr)
                {
                    self.asset_path_refs.insert(joined);
                }
                self.visit_expr(&binop.left, taints);
                self.visit_expr(&binop.right, taints);
            }
            Expr::Subscript(sub) => {
                let base = dotted_expr_name(&sub.value).unwrap_or_default();
                if (base == "os.environ" || base == "environ")
                    && let Expr::Constant(c) = sub.slice.as_ref()
                    && let Constant::Str(ref k) = c.value
                {
                    self.env_vars_read.insert(k.clone());
                }
                self.visit_expr(&sub.value, taints);
                self.visit_expr(&sub.slice, taints);
            }
            Expr::Constant(c) => {
                if let Constant::Str(ref s) = c.value {
                    self.inspect_string_literal(s);
                }
            }
            Expr::Attribute(a) => {
                if a.attr.as_str() == "__doc__" {
                    self.uses_source_introspection = true;
                }
                self.visit_expr(&a.value, taints);
            }
            Expr::BoolOp(b) => {
                for v in &b.values {
                    self.visit_expr(v, taints);
                }
            }
            Expr::NamedExpr(n) => {
                self.visit_expr(&n.target, taints);
                self.visit_expr(&n.value, taints);
            }
            Expr::UnaryOp(u) => self.visit_expr(&u.operand, taints),
            Expr::Lambda(l) => self.visit_expr(&l.body, taints),
            Expr::IfExp(i) => {
                self.visit_expr(&i.test, taints);
                self.visit_expr(&i.body, taints);
                self.visit_expr(&i.orelse, taints);
            }
            Expr::Dict(d) => {
                let is_settings_or_conftest = self.is_conftest
                    || self.rel_path.ends_with("/settings.py")
                    || self.rel_path.ends_with("_settings.py")
                    || self.rel_path.ends_with("/__init__.py");
                for (k, v) in d.keys.iter().zip(d.values.iter()) {
                    if let Some(ke) = k {
                        if !is_settings_or_conftest
                            && let Expr::Constant(kc) = ke
                            && let Constant::Str(ref key_str) = kc.value
                            && matches!(
                                key_str.as_str(),
                                "BACKEND" | "ROOT_URLCONF" | "DJANGO_SETTINGS_MODULE"
                            )
                            && let Expr::Constant(vc) = v
                            && let Constant::Str(ref vs) = vc.value
                            && looks_like_dotted_module(vs)
                        {
                            self.string_module_refs.insert(vs.clone());
                        }
                        self.visit_expr(ke, taints);
                    }
                    self.visit_expr(v, taints);
                }
            }
            Expr::Set(s) => {
                for e in &s.elts {
                    self.visit_expr(e, taints);
                }
            }
            Expr::List(l) => {
                for e in &l.elts {
                    self.visit_expr(e, taints);
                }
            }
            Expr::Tuple(t) => {
                for e in &t.elts {
                    self.visit_expr(e, taints);
                }
            }
            Expr::ListComp(c) => {
                self.visit_expr(&c.elt, taints);
                for g in &c.generators {
                    self.visit_comprehension(g, taints);
                }
            }
            Expr::SetComp(c) => {
                self.visit_expr(&c.elt, taints);
                for g in &c.generators {
                    self.visit_comprehension(g, taints);
                }
            }
            Expr::DictComp(c) => {
                self.visit_expr(&c.key, taints);
                self.visit_expr(&c.value, taints);
                for g in &c.generators {
                    self.visit_comprehension(g, taints);
                }
            }
            Expr::GeneratorExp(c) => {
                self.visit_expr(&c.elt, taints);
                for g in &c.generators {
                    self.visit_comprehension(g, taints);
                }
            }
            Expr::Await(a) => self.visit_expr(&a.value, taints),
            Expr::Yield(y) => {
                if let Some(v) = &y.value {
                    self.visit_expr(v, taints);
                }
            }
            Expr::YieldFrom(y) => self.visit_expr(&y.value, taints),
            Expr::Compare(c) => {
                self.visit_expr(&c.left, taints);
                for comp in &c.comparators {
                    self.visit_expr(comp, taints);
                }
            }
            Expr::FormattedValue(fv) => {
                self.visit_expr(&fv.value, taints);
                if let Some(fs) = &fv.format_spec {
                    self.visit_expr(fs, taints);
                }
            }
            Expr::JoinedStr(js) => {
                for v in &js.values {
                    self.visit_expr(v, taints);
                }
            }
            Expr::Starred(s) => self.visit_expr(&s.value, taints),
            Expr::Slice(s) => {
                if let Some(l) = &s.lower {
                    self.visit_expr(l, taints);
                }
                if let Some(u) = &s.upper {
                    self.visit_expr(u, taints);
                }
                if let Some(st) = &s.step {
                    self.visit_expr(st, taints);
                }
            }
            Expr::Name(_) => {}
        }
    }

    fn visit_comprehension(&mut self, g: &Comprehension, taints: &mut Vec<String>) {
        self.visit_expr(&g.iter, taints);
        for cond in &g.ifs {
            self.visit_expr(cond, taints);
        }
    }

    fn check_import_signals(&mut self, module: Option<&str>, names: &[String]) {
        let mut check_one = |m: &str| {
            if m.starts_with("django.contrib.admin") {
                self.uses_django_admin = true;
            }
            if m.starts_with("django.urls")
                || m.starts_with("django.template")
                || m == "django.test"
            {
                self.uses_django_client_or_url = true;
            }
            if m.starts_with("django.conf") {
                self.uses_django_settings = true;
            }
            if m.starts_with("django.tasks") {
                self.uses_django_tasks = true;
            }
            if m.starts_with("django.db.migrations") {
                self.uses_migration_executor = true;
                self.uses_django_db_module = true;
            }
        };
        if let Some(m) = module {
            check_one(m);
            if m == "django.test" {
                for n in names {
                    if matches!(
                        n.as_str(),
                        "Client" | "RequestFactory" | "TestCase" | "TransactionTestCase"
                    ) {
                        self.uses_django_client_or_url = true;
                    }
                    if matches!(n.as_str(), "override_settings" | "modify_settings") {
                        self.uses_django_settings = true;
                    }
                }
            }
        } else {
            for n in names {
                check_one(n);
            }
        }
    }

    fn inspect_string_literal(&mut self, s: &str) {
        let s = s.trim();
        if s.is_empty() || s.len() > 220 || s.contains('\n') || s.contains(' ') {
            return;
        }
        if s.contains('/')
            && !s.starts_with("http://")
            && !s.starts_with("https://")
            && !s.starts_with("ray://")
            && !s.starts_with('/')
        {
            if s.starts_with(".github/")
                || s.starts_with("docs/")
                || s.starts_with("k8s/")
                || s.starts_with("mk/")
                || s.starts_with("scripts/")
                || s.starts_with("src/")
                || s.starts_with("tests/")
                || s.starts_with("testproject/")
                || s.starts_with("qualification/")
                || s.ends_with(".yml")
                || s.ends_with(".yaml")
                || s.ends_with(".toml")
                || s.ends_with(".json")
                || s.ends_with(".mjs")
                || s.ends_with(".html")
                || s.ends_with(".md")
            {
                self.asset_path_refs.insert(s.to_string());
            }
        } else if matches!(
            s,
            "pyproject.toml"
                | "uv.lock"
                | "Makefile"
                | "README.md"
                | "CONTRIBUTING.md"
                | "LICENSE"
                | ".pyrn.toml"
                | "zensical.toml"
                | "Dockerfile"
                | "docker-compose.yml"
                | ".python-version"
        ) || s.ends_with(".yml")
            || s.ends_with(".yaml")
            || s.ends_with(".mjs")
            || s.ends_with(".toml")
        {
            self.asset_path_refs.insert(s.to_string());
        }

        let is_lazy_config_file = self.is_conftest
            || self.rel_path.ends_with("/settings.py")
            || self.rel_path.ends_with("_settings.py")
            || self.rel_path.ends_with("/__init__.py");
        if !is_lazy_config_file
            && looks_like_dotted_module(s)
            && s.split('.').count() >= 2
            && !s.starts_with("django.")
            && !s.starts_with("pytest.")
            && !s.starts_with("unittest.")
        {
            self.string_module_refs.insert(s.to_string());
        }
    }
}

fn extract_path_div_chain(expr: &Expr) -> Option<String> {
    let mut parts = Vec::new();
    let mut cur = expr;
    while let Expr::BinOp(b) = cur {
        if !matches!(b.op, Operator::Div) {
            break;
        }
        if let Expr::Constant(c) = b.right.as_ref()
            && let Constant::Str(ref s) = c.value
        {
            parts.push(s.as_str());
            cur = &b.left;
            continue;
        }
        return None;
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some(parts.join("/"))
}

fn dotted_expr_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(n) => Some(n.id.to_string()),
        Expr::Attribute(a) => {
            let prefix = dotted_expr_name(&a.value)?;
            Some(format!("{prefix}.{}", a.attr))
        }
        _ => None,
    }
}

fn looks_like_dotted_module(s: &str) -> bool {
    if !s.contains('.') || s.starts_with('.') || s.ends_with('.') || s.contains('/') {
        return false;
    }
    s.split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

fn extract_param_names(args: &Arguments) -> Vec<String> {
    let mut out = Vec::new();
    for a in &args.posonlyargs {
        if a.def.arg.as_str() != "self" && a.def.arg.as_str() != "cls" {
            out.push(a.def.arg.to_string());
        }
    }
    for a in &args.args {
        if a.def.arg.as_str() != "self" && a.def.arg.as_str() != "cls" {
            out.push(a.def.arg.to_string());
        }
    }
    for a in &args.kwonlyargs {
        out.push(a.def.arg.to_string());
    }
    out
}

fn extract_markers_from_expr(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Call(c) => {
            if let Some(name) = dotted_expr_name(&c.func)
                && let Some(mark) = name.strip_prefix("pytest.mark.")
                && let Some(first) = mark.split('.').next()
            {
                out.push(first.to_string());
            }
        }
        Expr::Attribute(_) => {
            if let Some(name) = dotted_expr_name(expr)
                && let Some(mark) = name.strip_prefix("pytest.mark.")
                && let Some(first) = mark.split('.').next()
            {
                out.push(first.to_string());
            }
        }
        Expr::List(l) => {
            for e in &l.elts {
                extract_markers_from_expr(e, out);
            }
        }
        Expr::Tuple(t) => {
            for e in &t.elts {
                extract_markers_from_expr(e, out);
            }
        }
        _ => {}
    }
}

fn extract_fixture_decorator(expr: &Expr) -> Option<(String, bool)> {
    match expr {
        Expr::Attribute(_) | Expr::Name(_) => {
            let name = dotted_expr_name(expr)?;
            if name == "pytest.fixture" || name == "fixture" {
                return Some(("function".to_string(), false));
            }
            None
        }
        Expr::Call(c) => {
            let name = dotted_expr_name(&c.func)?;
            if name != "pytest.fixture" && name != "fixture" {
                return None;
            }
            let mut scope = "function".to_string();
            let mut autouse = false;
            for kw in &c.keywords {
                if let Some(ref arg) = kw.arg {
                    if arg.as_str() == "scope" {
                        if let Expr::Constant(val) = &kw.value
                            && let Constant::Str(ref s) = val.value
                        {
                            scope = s.clone();
                        }
                    } else if arg.as_str() == "autouse"
                        && let Expr::Constant(val) = &kw.value
                        && let Constant::Bool(b) = val.value
                    {
                        autouse = b;
                    }
                }
            }
            Some((scope, autouse))
        }
        _ => None,
    }
}

fn collect_string_literals(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Constant(c) => {
            if let Constant::Str(ref s) = c.value {
                out.push(s.clone());
            }
        }
        Expr::Tuple(t) => {
            for e in &t.elts {
                collect_string_literals(e, out);
            }
        }
        Expr::List(l) => {
            for e in &l.elts {
                collect_string_literals(e, out);
            }
        }
        _ => {}
    }
}

fn is_cache_reset_fixture_body(body: &[Stmt]) -> bool {
    if body.is_empty() {
        return false;
    }
    let mut saw_try = false;
    for stmt in body {
        match stmt {
            Stmt::Expr(e) if matches!(e.value.as_ref(), Expr::Constant(_)) => continue,
            Stmt::Try(t) => {
                saw_try = true;
                if !t.orelse.is_empty() || !t.finalbody.is_empty() {
                    return false;
                }
                let handlers_ok = t.handlers.len() == 1
                    && t.handlers.iter().all(|h| {
                        let ExceptHandler::ExceptHandler(eh) = h;
                        eh.body.iter().all(|s| matches!(s, Stmt::Pass(_)))
                    });
                if !handlers_ok {
                    return false;
                }
                let body_ok = t.body.iter().all(|s| match s {
                    Stmt::Import(_) | Stmt::ImportFrom(_) => true,
                    Stmt::Assign(a) => {
                        matches!(a.value.as_ref(), Expr::Constant(c) if matches!(c.value, Constant::None))
                            && a.targets.iter().all(|tgt| matches!(tgt, Expr::Attribute(_)))
                    }
                    _ => false,
                });
                if !body_ok {
                    return false;
                }
            }
            _ => return false,
        }
    }
    saw_try
}

fn is_db_conditional_fixture_body(body: &[Stmt]) -> bool {
    let mut has_db_guard = false;
    let mut has_early_return = false;
    for stmt in body {
        if let Stmt::Assign(a) = stmt {
            let mut lits = Vec::new();
            collect_string_literals(&a.value, &mut lits);
            if let Expr::Set(s) = a.value.as_ref() {
                for e in &s.elts {
                    collect_string_literals(e, &mut lits);
                }
            }
            if lits
                .iter()
                .any(|s| s == "django_db" || s == "transactional_db")
            {
                has_db_guard = true;
            }
        }
        if let Stmt::If(i) = stmt
            && i.body.iter().any(|s| matches!(s, Stmt::Return(_)))
        {
            has_early_return = true;
        }
    }
    has_db_guard && has_early_return
}

// ============================================================================
// Zero-allocation canonical AST BLAKE3 hasher (comment/whitespace/formatting invariant)
// ============================================================================

fn compute_suite_ast_hash(suite: &[Stmt]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN_AST_BLOB.as_bytes());
    hasher.update(b"\0");
    hash_stmts(&mut hasher, suite);
    hasher.finalize().to_hex().to_string()
}

fn compute_stmt_ast_hash(stmt: &Stmt) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN_AST_SYMBOL.as_bytes());
    hasher.update(b"\0");
    hash_stmt(&mut hasher, stmt);
    hasher.finalize().to_hex().to_string()
}

fn hash_tag(h: &mut blake3::Hasher, tag: &[u8]) {
    h.update(tag);
    h.update(b":");
}

fn hash_str(h: &mut blake3::Hasher, s: &str) {
    h.update(&(s.len() as u32).to_le_bytes());
    h.update(s.as_bytes());
}

fn hash_stmts(h: &mut blake3::Hasher, stmts: &[Stmt]) {
    h.update(&(stmts.len() as u32).to_le_bytes());
    for s in stmts {
        hash_stmt(h, s);
    }
}

fn hash_opt_expr(h: &mut blake3::Hasher, e: Option<&Expr>) {
    match e {
        Some(expr) => {
            h.update(&[1]);
            hash_expr(h, expr);
        }
        None => {
            h.update(&[0]);
        }
    }
}

fn hash_args(h: &mut blake3::Hasher, args: &Arguments) {
    hash_arg_defaults(h, &args.posonlyargs);
    hash_arg_defaults(h, &args.args);
    if let Some(va) = &args.vararg {
        h.update(&[1]);
        hash_str(h, &va.arg);
        hash_opt_expr(h, va.annotation.as_deref());
    } else {
        h.update(&[0]);
    }
    hash_arg_defaults(h, &args.kwonlyargs);
    if let Some(ka) = &args.kwarg {
        h.update(&[1]);
        hash_str(h, &ka.arg);
        hash_opt_expr(h, ka.annotation.as_deref());
    } else {
        h.update(&[0]);
    }
}

fn hash_arg_defaults(h: &mut blake3::Hasher, items: &[ArgWithDefault]) {
    h.update(&(items.len() as u32).to_le_bytes());
    for a in items {
        hash_str(h, &a.def.arg);
        hash_opt_expr(h, a.def.annotation.as_deref());
        hash_opt_expr(h, a.default.as_deref());
    }
}

fn hash_keywords(h: &mut blake3::Hasher, kws: &[Keyword]) {
    h.update(&(kws.len() as u32).to_le_bytes());
    for k in kws {
        match &k.arg {
            Some(a) => {
                h.update(&[1]);
                hash_str(h, a);
            }
            None => {
                h.update(&[0]);
            }
        }
        hash_expr(h, &k.value);
    }
}

fn hash_op(h: &mut blake3::Hasher, op: Operator) {
    let b: u8 = match op {
        Operator::Add => 1,
        Operator::Sub => 2,
        Operator::Mult => 3,
        Operator::MatMult => 4,
        Operator::Div => 5,
        Operator::Mod => 6,
        Operator::Pow => 7,
        Operator::LShift => 8,
        Operator::RShift => 9,
        Operator::BitOr => 10,
        Operator::BitXor => 11,
        Operator::BitAnd => 12,
        Operator::FloorDiv => 13,
    };
    h.update(&[b]);
}

fn hash_boolop(h: &mut blake3::Hasher, op: ast::BoolOp) {
    h.update(&[match op {
        ast::BoolOp::And => 1,
        ast::BoolOp::Or => 2,
    }]);
}

fn hash_unaryop(h: &mut blake3::Hasher, op: ast::UnaryOp) {
    h.update(&[match op {
        ast::UnaryOp::Invert => 1,
        ast::UnaryOp::Not => 2,
        ast::UnaryOp::UAdd => 3,
        ast::UnaryOp::USub => 4,
    }]);
}

fn hash_cmpop(h: &mut blake3::Hasher, op: ast::CmpOp) {
    h.update(&[match op {
        ast::CmpOp::Eq => 1,
        ast::CmpOp::NotEq => 2,
        ast::CmpOp::Lt => 3,
        ast::CmpOp::LtE => 4,
        ast::CmpOp::Gt => 5,
        ast::CmpOp::GtE => 6,
        ast::CmpOp::Is => 7,
        ast::CmpOp::IsNot => 8,
        ast::CmpOp::In => 9,
        ast::CmpOp::NotIn => 10,
    }]);
}

fn hash_stmt(h: &mut blake3::Hasher, stmt: &Stmt) {
    match stmt {
        Stmt::FunctionDef(f) => {
            hash_tag(h, b"Fn");
            hash_str(h, &f.name);
            hash_args(h, &f.args);
            for d in &f.decorator_list {
                hash_expr(h, d);
            }
            hash_opt_expr(h, f.returns.as_deref());
            hash_stmts(h, &f.body);
        }
        Stmt::AsyncFunctionDef(f) => {
            hash_tag(h, b"AFn");
            hash_str(h, &f.name);
            hash_args(h, &f.args);
            for d in &f.decorator_list {
                hash_expr(h, d);
            }
            hash_opt_expr(h, f.returns.as_deref());
            hash_stmts(h, &f.body);
        }
        Stmt::ClassDef(c) => {
            hash_tag(h, b"Cls");
            hash_str(h, &c.name);
            for b in &c.bases {
                hash_expr(h, b);
            }
            hash_keywords(h, &c.keywords);
            for d in &c.decorator_list {
                hash_expr(h, d);
            }
            hash_stmts(h, &c.body);
        }
        Stmt::Return(r) => {
            hash_tag(h, b"Ret");
            hash_opt_expr(h, r.value.as_deref());
        }
        Stmt::Delete(d) => {
            hash_tag(h, b"Del");
            for t in &d.targets {
                hash_expr(h, t);
            }
        }
        Stmt::Assign(a) => {
            hash_tag(h, b"Asgn");
            for t in &a.targets {
                hash_expr(h, t);
            }
            hash_expr(h, &a.value);
        }
        Stmt::TypeAlias(ta) => {
            hash_tag(h, b"TypeAlias");
            hash_expr(h, &ta.name);
            hash_expr(h, &ta.value);
        }
        Stmt::AugAssign(a) => {
            hash_tag(h, b"AugAsgn");
            hash_expr(h, &a.target);
            hash_op(h, a.op);
            hash_expr(h, &a.value);
        }
        Stmt::AnnAssign(a) => {
            hash_tag(h, b"AnnAsgn");
            hash_expr(h, &a.target);
            hash_expr(h, &a.annotation);
            hash_opt_expr(h, a.value.as_deref());
        }
        Stmt::For(f) => {
            hash_tag(h, b"For");
            hash_expr(h, &f.target);
            hash_expr(h, &f.iter);
            hash_stmts(h, &f.body);
            hash_stmts(h, &f.orelse);
        }
        Stmt::AsyncFor(f) => {
            hash_tag(h, b"AFor");
            hash_expr(h, &f.target);
            hash_expr(h, &f.iter);
            hash_stmts(h, &f.body);
            hash_stmts(h, &f.orelse);
        }
        Stmt::While(w) => {
            hash_tag(h, b"While");
            hash_expr(h, &w.test);
            hash_stmts(h, &w.body);
            hash_stmts(h, &w.orelse);
        }
        Stmt::If(i) => {
            hash_tag(h, b"If");
            hash_expr(h, &i.test);
            hash_stmts(h, &i.body);
            hash_stmts(h, &i.orelse);
        }
        Stmt::With(w) => {
            hash_tag(h, b"With");
            for it in &w.items {
                hash_expr(h, &it.context_expr);
                hash_opt_expr(h, it.optional_vars.as_deref());
            }
            hash_stmts(h, &w.body);
        }
        Stmt::AsyncWith(w) => {
            hash_tag(h, b"AWith");
            for it in &w.items {
                hash_expr(h, &it.context_expr);
                hash_opt_expr(h, it.optional_vars.as_deref());
            }
            hash_stmts(h, &w.body);
        }
        Stmt::Match(m) => {
            hash_tag(h, b"Match");
            hash_expr(h, &m.subject);
            for c in &m.cases {
                hash_match_case(h, c);
            }
        }
        Stmt::Raise(r) => {
            hash_tag(h, b"Raise");
            hash_opt_expr(h, r.exc.as_deref());
            hash_opt_expr(h, r.cause.as_deref());
        }
        Stmt::Try(t) => {
            hash_tag(h, b"Try");
            hash_stmts(h, &t.body);
            for eh in &t.handlers {
                let ExceptHandler::ExceptHandler(e) = eh;
                hash_opt_expr(h, e.type_.as_deref());
                if let Some(n) = &e.name {
                    hash_str(h, n);
                }
                hash_stmts(h, &e.body);
            }
            hash_stmts(h, &t.orelse);
            hash_stmts(h, &t.finalbody);
        }
        Stmt::TryStar(t) => {
            hash_tag(h, b"TryStar");
            hash_stmts(h, &t.body);
            for eh in &t.handlers {
                let ExceptHandler::ExceptHandler(e) = eh;
                hash_opt_expr(h, e.type_.as_deref());
                if let Some(n) = &e.name {
                    hash_str(h, n);
                }
                hash_stmts(h, &e.body);
            }
            hash_stmts(h, &t.orelse);
            hash_stmts(h, &t.finalbody);
        }
        Stmt::Assert(a) => {
            hash_tag(h, b"Assert");
            hash_expr(h, &a.test);
            hash_opt_expr(h, a.msg.as_deref());
        }
        Stmt::Import(i) => {
            hash_tag(h, b"Imp");
            for a in &i.names {
                hash_str(h, &a.name);
                if let Some(asname) = &a.asname {
                    hash_str(h, asname);
                }
            }
        }
        Stmt::ImportFrom(i) => {
            hash_tag(h, b"ImpFrom");
            h.update(&(i.level.map(|l| l.to_u32()).unwrap_or(0)).to_le_bytes());
            if let Some(m) = &i.module {
                hash_str(h, m);
            }
            for a in &i.names {
                hash_str(h, &a.name);
                if let Some(asname) = &a.asname {
                    hash_str(h, asname);
                }
            }
        }
        Stmt::Global(g) => {
            hash_tag(h, b"Global");
            for n in &g.names {
                hash_str(h, n);
            }
        }
        Stmt::Nonlocal(n) => {
            hash_tag(h, b"Nonlocal");
            for name in &n.names {
                hash_str(h, name);
            }
        }
        Stmt::Expr(e) => {
            hash_tag(h, b"Expr");
            hash_expr(h, &e.value);
        }
        Stmt::Pass(_) => hash_tag(h, b"Pass"),
        Stmt::Break(_) => hash_tag(h, b"Break"),
        Stmt::Continue(_) => hash_tag(h, b"Cont"),
    }
}

fn hash_match_case(h: &mut blake3::Hasher, case: &MatchCase) {
    hash_pattern(h, &case.pattern);
    hash_opt_expr(h, case.guard.as_deref());
    hash_stmts(h, &case.body);
}

fn hash_pattern(h: &mut blake3::Hasher, pat: &Pattern) {
    match pat {
        Pattern::MatchValue(v) => {
            hash_tag(h, b"PMVal");
            hash_expr(h, &v.value);
        }
        Pattern::MatchSingleton(s) => {
            hash_tag(h, b"PMSing");
            hash_constant(h, &s.value);
        }
        Pattern::MatchSequence(s) => {
            hash_tag(h, b"PMSeq");
            for p in &s.patterns {
                hash_pattern(h, p);
            }
        }
        Pattern::MatchMapping(m) => {
            hash_tag(h, b"PMMap");
            for k in &m.keys {
                hash_expr(h, k);
            }
            for p in &m.patterns {
                hash_pattern(h, p);
            }
            if let Some(r) = &m.rest {
                hash_str(h, r);
            }
        }
        Pattern::MatchClass(c) => {
            hash_tag(h, b"PMCls");
            hash_expr(h, &c.cls);
            for p in &c.patterns {
                hash_pattern(h, p);
            }
            for a in &c.kwd_attrs {
                hash_str(h, a);
            }
            for p in &c.kwd_patterns {
                hash_pattern(h, p);
            }
        }
        Pattern::MatchStar(s) => {
            hash_tag(h, b"PMStar");
            if let Some(n) = &s.name {
                hash_str(h, n);
            }
        }
        Pattern::MatchAs(a) => {
            hash_tag(h, b"PMAs");
            if let Some(p) = &a.pattern {
                hash_pattern(h, p);
            }
            if let Some(n) = &a.name {
                hash_str(h, n);
            }
        }
        Pattern::MatchOr(o) => {
            hash_tag(h, b"PMOr");
            for p in &o.patterns {
                hash_pattern(h, p);
            }
        }
    }
}

fn hash_constant(h: &mut blake3::Hasher, c: &Constant) {
    match c {
        Constant::None => hash_tag(h, b"CNone"),
        Constant::Bool(b) => {
            hash_tag(h, b"CBool");
            h.update(&[u8::from(*b)]);
        }
        Constant::Str(s) => {
            hash_tag(h, b"CStr");
            hash_str(h, s);
        }
        Constant::Bytes(b) => {
            hash_tag(h, b"CBytes");
            h.update(&(b.len() as u32).to_le_bytes());
            h.update(b);
        }
        Constant::Int(i) => {
            hash_tag(h, b"CInt");
            hash_str(h, &i.to_string());
        }
        Constant::Tuple(t) => {
            hash_tag(h, b"CTup");
            for item in t {
                hash_constant(h, item);
            }
        }
        Constant::Float(f) => {
            hash_tag(h, b"CFlt");
            h.update(&f.to_bits().to_le_bytes());
        }
        Constant::Complex { real, imag } => {
            hash_tag(h, b"CCplx");
            h.update(&real.to_bits().to_le_bytes());
            h.update(&imag.to_bits().to_le_bytes());
        }
        Constant::Ellipsis => hash_tag(h, b"CEll"),
    }
}

fn hash_expr(h: &mut blake3::Hasher, expr: &Expr) {
    match expr {
        Expr::BoolOp(b) => {
            hash_tag(h, b"BOp");
            hash_boolop(h, b.op);
            for v in &b.values {
                hash_expr(h, v);
            }
        }
        Expr::NamedExpr(n) => {
            hash_tag(h, b"Named");
            hash_expr(h, &n.target);
            hash_expr(h, &n.value);
        }
        Expr::BinOp(b) => {
            hash_tag(h, b"BinOp");
            hash_expr(h, &b.left);
            hash_op(h, b.op);
            hash_expr(h, &b.right);
        }
        Expr::UnaryOp(u) => {
            hash_tag(h, b"UOp");
            hash_unaryop(h, u.op);
            hash_expr(h, &u.operand);
        }
        Expr::Lambda(l) => {
            hash_tag(h, b"Lam");
            hash_args(h, &l.args);
            hash_expr(h, &l.body);
        }
        Expr::IfExp(i) => {
            hash_tag(h, b"IfE");
            hash_expr(h, &i.test);
            hash_expr(h, &i.body);
            hash_expr(h, &i.orelse);
        }
        Expr::Dict(d) => {
            hash_tag(h, b"Dict");
            for (k, v) in d.keys.iter().zip(d.values.iter()) {
                hash_opt_expr(h, k.as_ref());
                hash_expr(h, v);
            }
        }
        Expr::Set(s) => {
            hash_tag(h, b"Set");
            for e in &s.elts {
                hash_expr(h, e);
            }
        }
        Expr::ListComp(c) => {
            hash_tag(h, b"LC");
            hash_expr(h, &c.elt);
            for g in &c.generators {
                hash_comp(h, g);
            }
        }
        Expr::SetComp(c) => {
            hash_tag(h, b"SC");
            hash_expr(h, &c.elt);
            for g in &c.generators {
                hash_comp(h, g);
            }
        }
        Expr::DictComp(c) => {
            hash_tag(h, b"DC");
            hash_expr(h, &c.key);
            hash_expr(h, &c.value);
            for g in &c.generators {
                hash_comp(h, g);
            }
        }
        Expr::GeneratorExp(c) => {
            hash_tag(h, b"GE");
            hash_expr(h, &c.elt);
            for g in &c.generators {
                hash_comp(h, g);
            }
        }
        Expr::Await(a) => {
            hash_tag(h, b"Await");
            hash_expr(h, &a.value);
        }
        Expr::Yield(y) => {
            hash_tag(h, b"Yield");
            hash_opt_expr(h, y.value.as_deref());
        }
        Expr::YieldFrom(y) => {
            hash_tag(h, b"YFrom");
            hash_expr(h, &y.value);
        }
        Expr::Compare(c) => {
            hash_tag(h, b"Cmp");
            hash_expr(h, &c.left);
            for op in &c.ops {
                hash_cmpop(h, *op);
            }
            for comp in &c.comparators {
                hash_expr(h, comp);
            }
        }
        Expr::Call(c) => {
            hash_tag(h, b"Call");
            hash_expr(h, &c.func);
            h.update(&(c.args.len() as u32).to_le_bytes());
            for a in &c.args {
                hash_expr(h, a);
            }
            hash_keywords(h, &c.keywords);
        }
        Expr::FormattedValue(fv) => {
            hash_tag(h, b"FVal");
            hash_expr(h, &fv.value);
            hash_opt_expr(h, fv.format_spec.as_deref());
        }
        Expr::JoinedStr(js) => {
            hash_tag(h, b"JStr");
            for v in &js.values {
                hash_expr(h, v);
            }
        }
        Expr::Constant(c) => hash_constant(h, &c.value),
        Expr::Attribute(a) => {
            hash_tag(h, b"Attr");
            hash_expr(h, &a.value);
            hash_str(h, &a.attr);
        }
        Expr::Subscript(s) => {
            hash_tag(h, b"Sub");
            hash_expr(h, &s.value);
            hash_expr(h, &s.slice);
        }
        Expr::Starred(s) => {
            hash_tag(h, b"Star");
            hash_expr(h, &s.value);
        }
        Expr::Name(n) => {
            hash_tag(h, b"Name");
            hash_str(h, &n.id);
        }
        Expr::List(l) => {
            hash_tag(h, b"List");
            h.update(&(l.elts.len() as u32).to_le_bytes());
            for e in &l.elts {
                hash_expr(h, e);
            }
        }
        Expr::Tuple(t) => {
            hash_tag(h, b"Tup");
            h.update(&(t.elts.len() as u32).to_le_bytes());
            for e in &t.elts {
                hash_expr(h, e);
            }
        }
        Expr::Slice(s) => {
            hash_tag(h, b"Slice");
            hash_opt_expr(h, s.lower.as_deref());
            hash_opt_expr(h, s.upper.as_deref());
            hash_opt_expr(h, s.step.as_deref());
        }
    }
}

fn hash_comp(h: &mut blake3::Hasher, g: &Comprehension) {
    hash_expr(h, &g.target);
    hash_expr(h, &g.iter);
    for cond in &g.ifs {
        hash_expr(h, cond);
    }
    h.update(&[u8::from(g.is_async)]);
}
