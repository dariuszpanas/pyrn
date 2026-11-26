//! Deterministic 64-hex BLAKE3 content-addressed cache-key engine (`pyrn.test-key.v1`).
//!
//! Pure in-memory (zero filesystem/SQLite coupling):
//! - Zero absolute paths in any cache key -> portable across commits, branches, and git worktrees.
//! - Symbol-aware closure hashing under `StrictImpacted`: when a dependency module `M` is reached
//!   ONLY via `SymbolFilter::Named(S)`, hashes `M.symbol_scoped_digest(S)` (preamble + intra-file closure
//!   of `S`), preserving mathematical duality with `DependencyGraph::compute_impact_with_symbols`.
//! - Automatically upgrades closures to `raw_hash` under `TransitiveImpacted`/`FullVerified` or whenever
//!   a test uses source introspection (`inspect.getsource`, `__doc__`, `ast.parse`) or parse fallback.
//! - Hashes both test-observed `os.environ` keys (static + dynamic proxy) AND allowlisted environment
//!   prefixes (e.g. `DJANGO_*`, `RAY_*`) from `PyrnConfig::env_allowlist_prefixes`.

use std::collections::{BTreeMap, BTreeSet};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::graph::{CacheSafety, DependencyGraph, SymbolFilter};
use crate::receipt::ConfidenceTier;
use crate::tree::DOMAIN_TEST_KEY;

/// Per-test computed content-addressed cache key + safety + closure diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCacheKeyRecord {
    pub nodeid: String,
    pub file_path: String,
    pub cache_key: String,
    pub safety: CacheSafety,
    pub closure_file_count: usize,
    pub uses_raw_hash: bool,
    pub uses_symbol_hashing: bool,
}

impl DependencyGraph {
    /// Compute deterministic 64-hex BLAKE3 content-addressed cache keys for every discovered test node.
    #[must_use]
    pub fn compute_all_cache_keys(
        &self,
        env_values: &BTreeMap<String, String>,
        tier: ConfidenceTier,
    ) -> Vec<TestCacheKeyRecord> {
        let force_raw_hashes = tier != ConfidenceTier::StrictImpacted;
        let symbol_aware = !force_raw_hashes && self.config.symbol_level_tia;

        // Collect allowlisted env var keys present in `env_values` (e.g., `DJANGO_*`, `RAY_*`)
        let allowlisted_env: Vec<(&str, &str)> = env_values
            .iter()
            .filter(|(k, _)| {
                self.config
                    .project
                    .env_allowlist_prefixes
                    .iter()
                    .any(|pref| k.starts_with(pref))
            })
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Compute transitive closures per test file once in parallel
        let test_files: Vec<String> = self.tests_by_file.keys().cloned().collect();
        let closures: BTreeMap<String, (usize, bool, bool, String)> = test_files
            .par_iter()
            .map(|tf| {
                let uses_raw = force_raw_hashes
                    || self
                        .py_files
                        .get(tf)
                        .map(|p| p.uses_source_introspection || p.parse_fallback)
                        .unwrap_or(true);
                let use_sym = symbol_aware && !uses_raw;
                let closure = self.transitive_deps_detailed(tf, use_sym);

                let mut hasher = blake3::Hasher::new();
                hasher.update(b"pyrn.closure.v1\0");
                hasher.update(if uses_raw {
                    b"raw\0"
                } else if use_sym {
                    b"sym\0"
                } else {
                    b"ast\0"
                });

                for (dep, sym_filter) in &closure {
                    hasher.update(dep.as_bytes());
                    hasher.update(b"=");
                    if uses_raw {
                        let d = self
                            .raw_hashes
                            .get(dep)
                            .map(String::as_str)
                            .unwrap_or("MISSING");
                        hasher.update(d.as_bytes());
                    } else if let Some(parsed) = self.py_files.get(dep) {
                        if parsed.parse_fallback {
                            hasher.update(parsed.raw_hash.as_bytes());
                        } else if use_sym {
                            match sym_filter {
                                SymbolFilter::All => {
                                    hasher.update(parsed.ast_hash.as_bytes());
                                }
                                SymbolFilter::Named(names) => {
                                    let scoped = parsed.symbol_scoped_digest(names);
                                    hasher.update(scoped.as_bytes());
                                }
                                SymbolFilter::PreambleOnly => {
                                    if dep.ends_with("/__init__.py") || dep == "__init__.py" {
                                        hasher.update(b"init:");
                                        hasher.update(parsed.ast_hash.as_bytes());
                                    } else {
                                        hasher.update(b"pre:");
                                        hasher.update(parsed.module_preamble_hash.as_bytes());
                                    }
                                }
                            }
                        } else {
                            hasher.update(parsed.ast_hash.as_bytes());
                        }
                    } else {
                        let d = self
                            .raw_hashes
                            .get(dep)
                            .map(String::as_str)
                            .unwrap_or("MISSING");
                        hasher.update(d.as_bytes());
                    }
                    hasher.update(b"\n");
                }
                let closure_digest = hasher.finalize().to_hex().to_string();
                (
                    tf.clone(),
                    (closure.len(), uses_raw, use_sym, closure_digest),
                )
            })
            .collect();

        let mut records = Vec::with_capacity(self.tests.len());
        for (idx, t) in self.tests.iter().enumerate() {
            let safety = self.test_safety[idx].clone();
            let (closure_len, uses_raw, use_sym, closure_digest) = closures
                .get(&t.file_path)
                .map(|(c, r, s, d)| (*c, *r, *s, d.as_str()))
                .unwrap_or((0, true, false, "EMPTY"));

            let mut key_hasher = blake3::Hasher::new();
            key_hasher.update(DOMAIN_TEST_KEY.as_bytes());
            key_hasher.update(b"\0");
            key_hasher.update(self.env_fingerprint.as_bytes());
            key_hasher.update(b"\0");
            key_hasher.update(tier.as_str().as_bytes());
            key_hasher.update(b"\0");
            key_hasher.update(t.nodeid.as_bytes());
            key_hasher.update(b"\0");
            key_hasher.update(t.ast_hash.as_bytes());
            key_hasher.update(b"\0");
            key_hasher.update(closure_digest.as_bytes());
            key_hasher.update(b"\0");

            // Hash allowlisted environment prefixes (`DJANGO_*`, `RAY_*`) deterministically
            let mut hashed_env_keys: BTreeSet<&str> = BTreeSet::new();
            for &(k, v) in &allowlisted_env {
                hashed_env_keys.insert(k);
                key_hasher.update(b"ALLOW:");
                key_hasher.update(k.as_bytes());
                key_hasher.update(b"=");
                key_hasher.update(v.as_bytes());
                key_hasher.update(b"\n");
            }

            // Hash explicitly read environment variables (static AST + dynamic `os.environ` proxy)
            if let CacheSafety::EnvDependentCacheable { ref env_keys } = safety {
                for k in env_keys {
                    if hashed_env_keys.contains(k.as_str()) {
                        continue;
                    }
                    key_hasher.update(b"READ:");
                    key_hasher.update(k.as_bytes());
                    key_hasher.update(b"=");
                    if let Some(v) = env_values.get(k) {
                        key_hasher.update(b"SET:");
                        key_hasher.update(v.as_bytes());
                    } else {
                        key_hasher.update(b"UNSET");
                    }
                    key_hasher.update(b"\n");
                }
            }

            records.push(TestCacheKeyRecord {
                nodeid: t.nodeid.clone(),
                file_path: t.file_path.clone(),
                cache_key: key_hasher.finalize().to_hex().to_string(),
                safety,
                closure_file_count: closure_len,
                uses_raw_hash: uses_raw,
                uses_symbol_hashing: use_sym,
            });
        }
        records
    }
}
