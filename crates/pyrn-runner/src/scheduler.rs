//! Resource- & Fixture-Affinity LPT Scheduler with dynamic work-stealing,
//! safe deterministic sub-file sharding for heavy-tail files, and historical timing persistence.

use crate::discovery::DiscoveredFile;
use crate::protocol::{ResourceLane, TestRecord};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

/// Persisted historical duration data (stored in external cache dir, NEVER in target repo).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimingHistory {
    /// EWMA duration in milliseconds per repo-relative test file path.
    pub file_ms: BTreeMap<String, f64>,
    /// EWMA duration in milliseconds per test nodeid.
    pub test_ms: BTreeMap<String, f64>,
}

impl TimingHistory {
    #[must_use]
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn update_and_save(&mut self, path: &Path, results: &[TestRecord]) {
        let mut per_file_sum: BTreeMap<String, f64> = BTreeMap::new();
        for r in results {
            let file_part = r
                .nodeid
                .split("::")
                .next()
                .unwrap_or(&r.nodeid)
                .replace('\\', "/");
            *per_file_sum.entry(file_part).or_insert(0.0) += r.total_ms;

            let prev = self.test_ms.get(&r.nodeid).copied();
            let updated = match prev {
                Some(old) => old * 0.6 + r.total_ms * 0.4,
                None => r.total_ms,
            };
            self.test_ms
                .insert(r.nodeid.clone(), (updated * 100.0).round() / 100.0);
        }

        for (file, ms) in per_file_sum {
            let prev = self.file_ms.get(&file).copied();
            let updated = match prev {
                Some(old) => old * 0.6 + ms * 0.4,
                None => ms,
            };
            self.file_ms.insert(file, (updated * 100.0).round() / 100.0);
        }

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(&tmp, path);
            }
        }
    }
}

/// Atomic unit of work scheduled to a worker.
#[derive(Clone, Debug)]
pub struct WorkUnit {
    pub unit_id: String,
    pub lane: ResourceLane,
    pub paths: Vec<String>,
    pub shard_index: usize,
    pub shard_total: usize,
    pub explicit_nodeids: Option<Vec<String>>,
    pub exclude_nodeids: Option<Vec<String>>,
    pub estimated_ms: f64,
    pub retry_attempt: u32,
}

/// Affinity-partitioned LPT work-stealing queue.
#[derive(Debug)]
pub struct AffinityWorkQueue {
    hermetic: VecDeque<WorkUnit>,
    sqlite_django: VecDeque<WorkUnit>,
    serial_external: VecDeque<WorkUnit>,
}

impl AffinityWorkQueue {
    /// Build LPT-ordered work units from discovered files, optionally applying a negative
    /// skip-list `exclude_nodeids_by_file` (for verified CAS hits while preserving inherited and
    /// dynamically generated tests) or explicit allowlist `explicit_nodeids_by_file`, and sharding
    /// heavy-tail files when `shard_heavy` is true.
    pub fn build(
        files: &[DiscoveredFile],
        history: &TimingHistory,
        workers: usize,
        shard_heavy: bool,
        explicit_nodeids_by_file: Option<&BTreeMap<String, Vec<String>>>,
        exclude_nodeids_by_file: Option<&BTreeMap<String, Vec<String>>>,
    ) -> (Self, usize, usize) {
        let mut hermetic_units = Vec::new();
        let mut sqlite_units = Vec::new();
        let mut serial_units = Vec::new();

        let mut next_id = 1usize;

        for f in files {
            let explicit = explicit_nodeids_by_file.and_then(|m| m.get(&f.rel_path));
            if let Some(explicit_map) = explicit_nodeids_by_file
                && !explicit_map.contains_key(&f.rel_path)
            {
                continue;
            }
            if let Some(nids) = explicit
                && nids.is_empty()
            {
                continue;
            }

            let excluded = exclude_nodeids_by_file
                .and_then(|m| m.get(&f.rel_path))
                .filter(|v| !v.is_empty())
                .cloned();

            let est_ms = estimate_file_ms(f, history);

            // Split large explicit nodeid subsets cleanly across shards too.
            if let Some(nids) = explicit {
                let shards = if shard_heavy
                    && workers >= 2
                    && f.lane != ResourceLane::SerialExternal
                    && !f.has_module_fixture
                    && nids.len() >= 8
                {
                    compute_shard_count(f, est_ms, workers)
                        .min(nids.len() / 4)
                        .max(1)
                } else {
                    1
                };
                for shard_idx in 0..shards {
                    let subset: Vec<String> = if shards == 1 {
                        nids.clone()
                    } else {
                        nids.iter()
                            .enumerate()
                            .filter(|(idx, _)| idx % shards == shard_idx)
                            .map(|(_, s)| s.clone())
                            .collect()
                    };
                    if subset.is_empty() {
                        continue;
                    }
                    let unit = WorkUnit {
                        unit_id: format!("u{next_id:04}"),
                        lane: f.lane,
                        paths: vec![f.rel_path.clone()],
                        shard_index: 0,
                        shard_total: 1,
                        explicit_nodeids: Some(subset),
                        exclude_nodeids: excluded.clone(),
                        estimated_ms: est_ms / (shards as f64),
                        retry_attempt: 0,
                    };
                    next_id += 1;
                    match f.lane {
                        ResourceLane::Hermetic => hermetic_units.push(unit),
                        ResourceLane::SqliteDjango => sqlite_units.push(unit),
                        ResourceLane::SerialExternal => serial_units.push(unit),
                    }
                }
                continue;
            }

            let shards = if shard_heavy
                && workers >= 2
                && f.lane != ResourceLane::SerialExternal
                && !f.has_module_fixture
            {
                compute_shard_count(f, est_ms, workers)
            } else {
                1
            };

            for shard_idx in 0..shards {
                let unit = WorkUnit {
                    unit_id: format!("u{next_id:04}"),
                    lane: f.lane,
                    paths: vec![f.rel_path.clone()],
                    shard_index: shard_idx,
                    shard_total: shards,
                    explicit_nodeids: None,
                    exclude_nodeids: excluded.clone(),
                    estimated_ms: est_ms / (shards as f64),
                    retry_attempt: 0,
                };
                next_id += 1;
                match f.lane {
                    ResourceLane::Hermetic => hermetic_units.push(unit),
                    ResourceLane::SqliteDjango => sqlite_units.push(unit),
                    ResourceLane::SerialExternal => serial_units.push(unit),
                }
            }
        }

        // LPT ordering: longest estimated duration first so heavy-tail items start at t=0.
        let lpt_cmp = |a: &WorkUnit, b: &WorkUnit| {
            b.estimated_ms
                .total_cmp(&a.estimated_ms)
                .then_with(|| a.unit_id.cmp(&b.unit_id))
        };
        hermetic_units.sort_by(lpt_cmp);
        sqlite_units.sort_by(lpt_cmp);
        serial_units.sort_by(lpt_cmp);

        let total_hermetic = hermetic_units.len();
        let total_sqlite = sqlite_units.len();

        let queue = Self {
            hermetic: VecDeque::from(hermetic_units),
            sqlite_django: VecDeque::from(sqlite_units),
            serial_external: VecDeque::from(serial_units),
        };
        (queue, total_hermetic, total_sqlite)
    }

    /// Pop next work unit for a worker with lane affinity, warm-module sticky preference,
    /// and dynamic work-stealing.
    pub fn pop_for_sticky(
        &mut self,
        worker_lane: ResourceLane,
        warm_files: &HashSet<String>,
    ) -> Option<WorkUnit> {
        let pop_sticky = |dq: &mut VecDeque<WorkUnit>| -> Option<WorkUnit> {
            if !warm_files.is_empty() && dq.len() > 1 {
                let window = dq.len().min(16);
                if let Some(pos) = dq
                    .iter()
                    .take(window)
                    .position(|u| u.paths.iter().all(|p| warm_files.contains(p)))
                {
                    return dq.remove(pos);
                }
            }
            dq.pop_front()
        };

        match worker_lane {
            ResourceLane::SqliteDjango => {
                pop_sticky(&mut self.sqlite_django).or_else(|| pop_sticky(&mut self.hermetic))
            }
            ResourceLane::Hermetic => {
                pop_sticky(&mut self.hermetic).or_else(|| pop_sticky(&mut self.sqlite_django))
            }
            ResourceLane::SerialExternal => self.serial_external.pop_front(),
        }
    }

    /// Push a recovery/rerun unit to the front of its affinity queue so stragglers resolve fast.
    pub fn push_front(&mut self, unit: WorkUnit) {
        match unit.lane {
            ResourceLane::Hermetic => self.hermetic.push_front(unit),
            ResourceLane::SqliteDjango => self.sqlite_django.push_front(unit),
            ResourceLane::SerialExternal => self.serial_external.push_front(unit),
        }
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.hermetic.len() + self.sqlite_django.len() + self.serial_external.len()
    }

    #[must_use]
    pub fn has_serial_external(&self) -> bool {
        !self.serial_external.is_empty()
    }
}

/// Decide optimal worker split `(hermetic_workers, sqlite_workers)` proportional to estimated work.
#[must_use]
pub fn plan_worker_split(
    files: &[DiscoveredFile],
    history: &TimingHistory,
    total_workers: usize,
    herm_units: usize,
    sqlite_units: usize,
) -> (usize, usize) {
    let w = total_workers.max(1);
    let mut herm_est = 0.0f64;
    let mut db_est = 0.0f64;

    for f in files {
        let ms = estimate_file_ms(f, history);
        match f.lane {
            ResourceLane::Hermetic => herm_est += ms,
            ResourceLane::SqliteDjango => db_est += ms,
            ResourceLane::SerialExternal => {}
        }
    }

    if sqlite_units == 0 {
        return (w.min(herm_units.max(1)), 0);
    }
    if herm_units == 0 {
        return (0, w.min(sqlite_units.max(1)));
    }
    if w == 1 {
        return (0, 1);
    }

    let total_est = (herm_est + db_est).max(1.0);
    let db_share = ((db_est / total_est) * (w as f64)).round() as usize;
    let sqlite_w = db_share.clamp(1, (w - 1).min(sqlite_units.max(1)));
    let herm_w = (w - sqlite_w).max(1);
    (herm_w, sqlite_w)
}

fn estimate_file_ms(f: &DiscoveredFile, history: &TimingHistory) -> f64 {
    if let Some(&hist_ms) = history.file_ms.get(&f.rel_path) {
        return hist_ms.max(10.0);
    }
    let per_test = match f.lane {
        ResourceLane::Hermetic => 1.5,
        ResourceLane::SqliteDjango => 22.0,
        ResourceLane::SerialExternal => 500.0,
    };
    let import_prior = (f.line_count as f64) * 0.08;
    (f.estimated_tests as f64 * per_test + import_prior).max(15.0)
}

fn compute_shard_count(f: &DiscoveredFile, est_ms: f64, workers: usize) -> usize {
    let max_shards = (workers / 2).clamp(2, 8);
    if f.lane == ResourceLane::SqliteDjango {
        if est_ms >= 12_000.0 {
            return max_shards;
        }
        if est_ms >= 3000.0 || f.line_count >= 2500 {
            return 4.min(max_shards);
        }
        if est_ms >= 1500.0 || f.line_count >= 1200 {
            return 2.min(max_shards);
        }
        return 1;
    }
    if est_ms >= 12_000.0 {
        return max_shards;
    }
    if est_ms >= 4000.0 || f.line_count >= 3500 {
        return 4.min(max_shards);
    }
    if est_ms >= 1800.0 || f.line_count >= 1800 {
        return 2.min(max_shards);
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn scheduler_lpt_sharding_and_sticky_affinity() {
        let files = vec![
            DiscoveredFile {
                rel_path: "tests/unit/test_small.py".to_owned(),
                abs_path: PathBuf::from("tests/unit/test_small.py"),
                lane: ResourceLane::Hermetic,
                estimated_tests: 5,
                line_count: 50,
                has_module_fixture: false,
            },
            DiscoveredFile {
                rel_path: "tests/unit/test_big.py".to_owned(),
                abs_path: PathBuf::from("tests/unit/test_big.py"),
                lane: ResourceLane::Hermetic,
                estimated_tests: 100,
                line_count: 800,
                has_module_fixture: false,
            },
            DiscoveredFile {
                rel_path: "tests/integration/test_db.py".to_owned(),
                abs_path: PathBuf::from("tests/integration/test_db.py"),
                lane: ResourceLane::SqliteDjango,
                estimated_tests: 50,
                line_count: 1500,
                has_module_fixture: false,
            },
        ];

        let history = TimingHistory::default();
        let (mut q, h_units, db_units) =
            AffinityWorkQueue::build(&files, &history, 8, true, None, None);
        assert_eq!(h_units, 2);
        assert!(db_units >= 2);

        let mut warm = HashSet::new();
        warm.insert("tests/unit/test_small.py".to_owned());
        let u1 = q.pop_for_sticky(ResourceLane::Hermetic, &warm).expect("u1");
        assert_eq!(u1.paths[0], "tests/unit/test_small.py");

        let u2 = q
            .pop_for_sticky(ResourceLane::SqliteDjango, &HashSet::new())
            .expect("u2");
        assert_eq!(u2.lane, ResourceLane::SqliteDjango);
    }
}
