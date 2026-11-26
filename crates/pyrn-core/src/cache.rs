//! Multi-process ACID SQLite WAL Content-Addressed Test Result Cache (`feature = "cache"`).
//!
//! Design guarantees:
//! - Keyed strictly by 64-hex domain-separated BLAKE3 `pyrn.test-key.v1` keys (zero absolute paths),
//!   enabling verified cross-commit and cross-worktree cache hits across git worktrees and branches.
//! - Segregated by `(SlotRole::{Agent, Verify}, project_fp, env_fingerprint)` so interactive
//!   development runs never bleed into clean verification pools.
//! - `lookup_passing_hits` strictly serves ONLY `outcome == "passed"` rows (failed, errored,
//!   skipped, flaky, or tainted entries are never returned as cache hits).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::receipt::{MAX_CRASH_CAPSULE_BYTES, SlotRole, truncate_utf8_bytes};

/// Stored verification entry for one test node under a specific content-addressed key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTestEntry {
    pub cache_key: String,
    pub role: SlotRole,
    pub project_fp: String,
    pub env_fingerprint: String,
    pub nodeid: String,
    pub outcome: String,
    pub duration_ms: f64,
    pub commit_sha: String,
    pub witness_128b: String,
    pub confidence_tier: String,
    pub created_at_epoch_s: i64,
    pub hit_count: i64,
}

/// Aggregate cache health statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    pub total_entries: i64,
    pub total_hits: i64,
    pub distinct_projects: i64,
    pub agent_entries: i64,
    pub verify_entries: i64,
    pub passed_entries: i64,
    pub failed_entries: i64,
    pub skipped_entries: i64,
}

/// SQLite WAL content-addressed test cache safe for concurrent multi-worktree access.
pub struct CasTestCache {
    conn: Connection,
}

impl CasTestCache {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create cache directory {}", parent.display())
            })?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open SQLite cache {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let cache = Self { conn };
        cache.init_schema()?;
        Ok(cache)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let cache = Self { conn };
        cache.init_schema()?;
        Ok(cache)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS test_cache (
                role TEXT NOT NULL,
                project_fp TEXT NOT NULL,
                env_fingerprint TEXT NOT NULL,
                cache_key TEXT NOT NULL,
                nodeid TEXT NOT NULL,
                outcome TEXT NOT NULL,
                duration_ms REAL NOT NULL,
                commit_sha TEXT NOT NULL,
                witness_128b TEXT NOT NULL,
                confidence_tier TEXT NOT NULL,
                created_at_epoch_s INTEGER NOT NULL,
                hit_count INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (role, project_fp, env_fingerprint, cache_key)
            );
            CREATE INDEX IF NOT EXISTS idx_test_cache_nodeid
                ON test_cache (role, project_fp, nodeid);
            CREATE INDEX IF NOT EXISTS idx_test_cache_created
                ON test_cache (created_at_epoch_s);
            "#,
        )?;
        Ok(())
    }

    /// Insert or update a batch of verified test outcomes inside one ACID SQLite transaction.
    /// Callers must only seed non-tainted tests; `put_passing_batch` enforces `outcome == "passed"`.
    pub fn put_batch(&mut self, entries: &[CachedTestEntry]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let now = now_epoch_s();
        let mut count = 0usize;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                INSERT INTO test_cache (
                    role, project_fp, env_fingerprint, cache_key, nodeid,
                    outcome, duration_ms, commit_sha, witness_128b,
                    confidence_tier, created_at_epoch_s, hit_count
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT(role, project_fp, env_fingerprint, cache_key) DO UPDATE SET
                    outcome = excluded.outcome,
                    duration_ms = excluded.duration_ms,
                    commit_sha = excluded.commit_sha,
                    witness_128b = excluded.witness_128b,
                    confidence_tier = excluded.confidence_tier,
                    created_at_epoch_s = excluded.created_at_epoch_s
                "#,
            )?;
            for e in entries {
                let witness = truncate_utf8_bytes(&e.witness_128b, MAX_CRASH_CAPSULE_BYTES);
                let ts = if e.created_at_epoch_s > 0 {
                    e.created_at_epoch_s
                } else {
                    now
                };
                stmt.execute(params![
                    e.role.as_str(),
                    e.project_fp,
                    e.env_fingerprint,
                    e.cache_key,
                    e.nodeid,
                    e.outcome,
                    e.duration_ms,
                    e.commit_sha,
                    witness,
                    e.confidence_tier,
                    ts,
                    e.hit_count,
                ])?;
                count += 1;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    /// Insert ONLY entries with `outcome == "passed"`, silently ignoring failed/skipped/flaky rows.
    pub fn put_passing_batch(&mut self, entries: &[CachedTestEntry]) -> Result<usize> {
        let passing: Vec<CachedTestEntry> = entries
            .iter()
            .filter(|e| e.outcome == "passed")
            .cloned()
            .collect();
        self.put_batch(&passing)
    }

    /// Look up one test's cache entry under `(role, project_fp, env_fingerprint, cache_key)`.
    pub fn lookup(
        &self,
        role: SlotRole,
        project_fp: &str,
        env_fingerprint: &str,
        cache_key: &str,
    ) -> Result<Option<CachedTestEntry>> {
        let row = self
            .conn
            .query_row(
                r#"
                SELECT role, project_fp, env_fingerprint, cache_key, nodeid,
                       outcome, duration_ms, commit_sha, witness_128b,
                       confidence_tier, created_at_epoch_s, hit_count
                FROM test_cache
                WHERE role = ?1 AND project_fp = ?2 AND env_fingerprint = ?3 AND cache_key = ?4
                "#,
                params![role.as_str(), project_fp, env_fingerprint, cache_key],
                row_to_entry,
            )
            .optional()?;
        Ok(row)
    }

    /// Batch lookup returning ONLY verified passing (`outcome = 'passed'`) cache hits and
    /// optionally incrementing `hit_count`. Failed, skipped, errored, or flaky rows are NEVER returned.
    pub fn lookup_passing_hits(
        &mut self,
        role: SlotRole,
        project_fp: &str,
        env_fingerprint: &str,
        keys: &[String],
        record_hits: bool,
    ) -> Result<BTreeMap<String, CachedTestEntry>> {
        let mut found = BTreeMap::new();
        if keys.is_empty() {
            return Ok(found);
        }
        let tx = self.conn.transaction()?;
        {
            let mut select_stmt = tx.prepare_cached(
                r#"
                SELECT role, project_fp, env_fingerprint, cache_key, nodeid,
                       outcome, duration_ms, commit_sha, witness_128b,
                       confidence_tier, created_at_epoch_s, hit_count
                FROM test_cache
                WHERE role = ?1 AND project_fp = ?2 AND env_fingerprint = ?3
                  AND cache_key = ?4 AND outcome = 'passed'
                "#,
            )?;
            let mut bump_stmt = tx.prepare_cached(
                r#"
                UPDATE test_cache
                SET hit_count = hit_count + 1
                WHERE role = ?1 AND project_fp = ?2 AND env_fingerprint = ?3
                  AND cache_key = ?4 AND outcome = 'passed'
                "#,
            )?;
            for key in keys {
                if let Some(entry) = select_stmt
                    .query_row(
                        params![role.as_str(), project_fp, env_fingerprint, key],
                        row_to_entry,
                    )
                    .optional()?
                    && entry.outcome == "passed"
                {
                    if record_hits {
                        bump_stmt.execute(params![
                            role.as_str(),
                            project_fp,
                            env_fingerprint,
                            key
                        ])?;
                    }
                    found.insert(key.clone(), entry);
                }
            }
        }
        tx.commit()?;
        Ok(found)
    }

    /// Delete cache entries older than `max_age_days` days (or all entries if `max_age_days == 0`).
    /// Returns the number of rows pruned.
    pub fn prune_older_than_days(
        &mut self,
        max_age_days: u64,
        role_filter: Option<SlotRole>,
    ) -> Result<usize> {
        let cutoff = if max_age_days == 0 {
            i64::MAX
        } else {
            now_epoch_s().saturating_sub((max_age_days * 86_400) as i64)
        };
        let removed = match role_filter {
            Some(role) => self.conn.execute(
                "DELETE FROM test_cache WHERE created_at_epoch_s <= ?1 AND role = ?2",
                params![cutoff, role.as_str()],
            )?,
            None => self.conn.execute(
                "DELETE FROM test_cache WHERE created_at_epoch_s <= ?1",
                params![cutoff],
            )?,
        };
        Ok(removed)
    }

    /// Return aggregate health statistics for `pyrn cache status`.
    pub fn stats(&self) -> Result<CacheStats> {
        let row = self.conn.query_row(
            r#"
            SELECT
                COUNT(*),
                COALESCE(SUM(hit_count), 0),
                COUNT(DISTINCT project_fp),
                COALESCE(SUM(CASE WHEN role = 'agent' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN role = 'verify' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN outcome = 'passed' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN outcome = 'failed' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN outcome = 'skipped' THEN 1 ELSE 0 END), 0)
            FROM test_cache
            "#,
            [],
            |r| {
                Ok(CacheStats {
                    total_entries: r.get(0)?,
                    total_hits: r.get(1)?,
                    distinct_projects: r.get(2)?,
                    agent_entries: r.get(3)?,
                    verify_entries: r.get(4)?,
                    passed_entries: r.get(5)?,
                    failed_entries: r.get(6)?,
                    skipped_entries: r.get(7)?,
                })
            },
        )?;
        Ok(row)
    }
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<CachedTestEntry> {
    let role_s: String = r.get(0)?;
    let role = SlotRole::parse(&role_s).unwrap_or(SlotRole::Agent);
    Ok(CachedTestEntry {
        role,
        project_fp: r.get(1)?,
        env_fingerprint: r.get(2)?,
        cache_key: r.get(3)?,
        nodeid: r.get(4)?,
        outcome: r.get(5)?,
        duration_ms: r.get(6)?,
        commit_sha: r.get(7)?,
        witness_128b: r.get(8)?,
        confidence_tier: r.get(9)?,
        created_at_epoch_s: r.get(10)?,
        hit_count: r.get(11)?,
    })
}

fn now_epoch_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
