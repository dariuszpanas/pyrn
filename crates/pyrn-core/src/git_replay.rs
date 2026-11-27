//! Read-only Git history replay and working-tree symbol diffing (`feature = "process"`).
//!
//! Computes exact symbol-level AST diffs (`ChangedFileSpec` with `ChangedSymbols::{None, Some, All}`)
//! between `sha^` and `sha` (or between `--base <ref>` and current working tree) without ever
//! modifying the target repository.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::PyrnConfig;
use crate::graph::{ChangedFileSpec, DependencyGraph};
use crate::parser::{ChangedSymbols, diff_python_symbols, parse_python_file};
use crate::receipt::ConfidenceTier;
use crate::tree::{DOMAIN_RAW_BLOB, blake3_domain_hex, normalize_rel_path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitReplayRow {
    pub sha: String,
    pub subject: String,
    pub changed_files: Vec<String>,
    pub prod_changed_files: Vec<String>,
    pub test_changed_files: Vec<String>,
    pub comment_or_format_only: Vec<String>,
    pub symbol_pruned_edges: usize,
    pub effective_tier: ConfidenceTier,
    pub full_fallback_reasons: Vec<String>,
    pub total_test_files: usize,
    pub impacted_test_files: usize,
    pub total_test_nodes: usize,
    pub impacted_test_nodes: usize,
    pub reduction_pct: f64,
    pub estimated_speedup: f64,
    pub duration_weighted_speedup: Option<f64>,
    pub eval_ms: f64,
    /// Co-change soundness check: when ONLY non-test diffs are fed into TIA,
    /// how many existing co-changed test files were predicted as impacted?
    pub cochange_existing_tests: usize,
    pub cochange_predicted_tests: usize,
    pub cochange_missed_tests: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaySummary {
    pub commits_analyzed: usize,
    pub tier: ConfidenceTier,
    pub symbol_level_tia: bool,
    pub has_dynamic_traces: bool,
    pub total_test_files: usize,
    pub total_test_nodes: usize,
    pub mean_impacted_files_pct: f64,
    pub median_impacted_nodes_pct: f64,
    pub mean_reduction_pct: f64,
    pub median_speedup: f64,
    pub median_duration_speedup: Option<f64>,
    pub mean_duration_reduction_pct: Option<f64>,
    pub total_suite_duration_ms: Option<f64>,
    pub full_fallback_commits: usize,
    pub zero_impact_commits: usize,
    pub total_cochange_existing_tests: usize,
    pub total_cochange_predicted_tests: usize,
    pub cochange_recall_pct: f64,
    pub rows: Vec<CommitReplayRow>,
}

/// Compute symbol-level `ChangedFileSpec`s for working-tree changes relative to `base_ref`
/// (e.g. `HEAD`, `main`, `origin/main`) in a strictly read-only manner.
///
/// When `base_ref != "HEAD"`, resolves the exact common ancestor via `git merge-base <base_ref> HEAD`
/// so branches behind `base_ref` (including GitHub Actions PR merge commits) diff only their own
/// changes rather than unrelated upstream commits on `base_ref`.
pub fn git_changed_specs_against_base(
    repo_root: &Path,
    base_ref: &str,
    config: &PyrnConfig,
) -> Result<Vec<ChangedFileSpec>> {
    let resolved_base = if base_ref == "HEAD" {
        "HEAD".to_owned()
    } else {
        let mb_out = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["merge-base", base_ref, "HEAD"])
            .output()
            .with_context(|| format!("failed to run git merge-base {base_ref} HEAD"))?;
        if !mb_out.status.success() {
            anyhow::bail!(
                "git merge-base {base_ref} HEAD failed: {}",
                String::from_utf8_lossy(&mb_out.stderr).trim()
            );
        }
        let sha = String::from_utf8_lossy(&mb_out.stdout).trim().to_owned();
        if sha.is_empty() {
            anyhow::bail!("git merge-base {base_ref} HEAD returned empty output");
        }
        sha
    };

    let diff_out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["diff", "--name-status", &resolved_base])
        .output()
        .with_context(|| format!("failed to run git diff against {resolved_base}"))?;
    if !diff_out.status.success() {
        anyhow::bail!(
            "git diff --name-status {resolved_base} failed: {}",
            String::from_utf8_lossy(&diff_out.stderr)
        );
    }

    let untracked_out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .context("failed to run git ls-files")?;

    let mut changed_paths: BTreeSet<String> = BTreeSet::new();
    let mut added_or_deleted: BTreeSet<String> = BTreeSet::new();

    for line in String::from_utf8_lossy(&diff_out.stdout).lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next() else {
            continue;
        };
        let Some(p1) = parts.next() else {
            continue;
        };
        let norm1 = normalize_rel_path(p1);
        if status.starts_with('A') || status.starts_with('D') || status.starts_with('R') {
            added_or_deleted.insert(norm1.clone());
        }
        changed_paths.insert(norm1);
        if let Some(p2) = parts.next() {
            let norm2 = normalize_rel_path(p2);
            added_or_deleted.insert(norm2.clone());
            changed_paths.insert(norm2);
        }
    }
    if untracked_out.status.success() {
        for line in String::from_utf8_lossy(&untracked_out.stdout).lines() {
            let norm = normalize_rel_path(line.trim());
            if !norm.is_empty() {
                added_or_deleted.insert(norm.clone());
                changed_paths.insert(norm);
            }
        }
    }

    let mut specs = Vec::new();
    for path in changed_paths {
        if path.ends_with(".py")
            && !added_or_deleted.contains(&path)
            && let (Some(before), Ok(after)) = (
                git_show_bytes(repo_root, &format!("{resolved_base}:{path}")),
                std::fs::read(repo_root.join(&path)),
            )
        {
            let raw_b = blake3_domain_hex(DOMAIN_RAW_BLOB, &before);
            let raw_a = blake3_domain_hex(DOMAIN_RAW_BLOB, &after);
            let pb = parse_python_file(&path, &before, &raw_b, config);
            let pa = parse_python_file(&path, &after, &raw_a, config);
            let syms = diff_python_symbols(&pb, &pa);
            specs.push(ChangedFileSpec::with_symbols(path, syms));
            continue;
        }
        specs.push(ChangedFileSpec::all(path));
    }
    Ok(specs)
}

/// Replay the latest `limit` non-merge commits in `repo_root` against `graph` (read-only!),
/// performing exact symbol-level AST diffing between `sha^` and `sha`.
pub fn replay_git_history(
    repo_root: &Path,
    graph: &DependencyGraph,
    limit: usize,
    tier: ConfidenceTier,
) -> Result<ReplaySummary> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "log",
            "--no-merges",
            &format!("-n{limit}"),
            "--pretty=format:%H\t%s",
        ])
        .output()
        .with_context(|| format!("failed to invoke git log in {}", repo_root.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    let total_known_duration_ms: f64 = graph.file_durations_ms.values().sum();

    for line in stdout.lines() {
        let Some((sha, subject)) = line.split_once('\t') else {
            continue;
        };
        let (changed_files, added_files) = git_commit_files(repo_root, sha)?;
        if changed_files.is_empty() {
            continue;
        }

        // Compute exact symbol-level AST diff between `sha^:f` and `sha:f`
        let mut specs = Vec::with_capacity(changed_files.len());
        let mut comment_only = Vec::new();

        for f in &changed_files {
            if f.ends_with(".py") && !added_files.contains(f) {
                let syms = diff_git_commit_python(repo_root, sha, f, &graph.config.project);
                if syms == ChangedSymbols::None {
                    comment_only.push(f.clone());
                }
                specs.push(ChangedFileSpec::with_symbols(f.clone(), syms));
            } else {
                specs.push(ChangedFileSpec::all(f.clone()));
            }
        }

        let impact = graph.compute_impact_with_symbols(&specs, tier);

        let prod_specs: Vec<ChangedFileSpec> = specs
            .iter()
            .filter(|s| !s.path.starts_with("tests/"))
            .cloned()
            .collect();
        let prod_changed: Vec<String> = prod_specs.iter().map(|s| s.path.clone()).collect();
        let test_changed: Vec<String> = changed_files
            .iter()
            .filter(|f| f.starts_with("tests/") && f.ends_with(".py"))
            .cloned()
            .collect();

        // Co-change soundness check: feed ONLY non-test symbol diffs into TIA!
        let mut cochange_existing = 0usize;
        let mut cochange_predicted = 0usize;
        let mut cochange_missed = Vec::new();
        if !prod_specs.is_empty() && !test_changed.is_empty() {
            let prod_impact = graph.compute_impact_with_symbols(&prod_specs, tier);
            let predicted_set: BTreeSet<&str> = prod_impact
                .impacted
                .iter()
                .map(|i| i.test_file.as_str())
                .collect();
            for tf in &test_changed {
                if !added_files.contains(tf) && graph.tests_by_file.contains_key(tf) {
                    cochange_existing += 1;
                    if predicted_set.contains(tf.as_str()) {
                        cochange_predicted += 1;
                    } else {
                        cochange_missed.push(tf.clone());
                    }
                }
            }
        }

        let estimated_speedup = if impact.impacted_test_nodes == 0 {
            impact.total_test_nodes.max(1) as f64
        } else {
            impact.total_test_nodes as f64 / impact.impacted_test_nodes as f64
        };

        let duration_weighted_speedup = if total_known_duration_ms > 0.0 {
            let impacted_dur: f64 = impact
                .impacted
                .iter()
                .filter_map(|i| graph.file_durations_ms.get(&i.test_file))
                .sum();
            let effective_ms = if impact.impacted_test_nodes == 0 {
                136.0
            } else {
                impacted_dur + 136.0 + 810.0
            };
            Some(total_known_duration_ms / effective_ms.max(1.0))
        } else {
            None
        };

        rows.push(CommitReplayRow {
            sha: sha[..sha.len().min(8)].to_string(),
            subject: subject.to_string(),
            changed_files,
            prod_changed_files: prod_changed,
            test_changed_files: test_changed,
            comment_or_format_only: comment_only,
            symbol_pruned_edges: impact.symbol_pruned_edges,
            effective_tier: impact.effective_tier,
            full_fallback_reasons: impact.full_fallback_reasons,
            total_test_files: impact.total_test_files,
            impacted_test_files: impact.impacted_test_files,
            total_test_nodes: impact.total_test_nodes,
            impacted_test_nodes: impact.impacted_test_nodes,
            reduction_pct: impact.reduction_pct,
            estimated_speedup,
            duration_weighted_speedup,
            eval_ms: impact.eval_ms,
            cochange_existing_tests: cochange_existing,
            cochange_predicted_tests: cochange_predicted,
            cochange_missed_tests: cochange_missed,
        });
    }

    let n = rows.len().max(1) as f64;
    let mean_impacted_files_pct = rows
        .iter()
        .map(|r| {
            if r.total_test_files == 0 {
                0.0
            } else {
                (r.impacted_test_files as f64 / r.total_test_files as f64) * 100.0
            }
        })
        .sum::<f64>()
        / n;
    let mean_reduction_pct = rows.iter().map(|r| r.reduction_pct).sum::<f64>() / n;

    let mut node_pcts: Vec<f64> = rows
        .iter()
        .map(|r| {
            if r.total_test_nodes == 0 {
                0.0
            } else {
                (r.impacted_test_nodes as f64 / r.total_test_nodes as f64) * 100.0
            }
        })
        .collect();
    node_pcts.sort_by(f64::total_cmp);
    let median_impacted_nodes_pct = node_pcts.get(node_pcts.len() / 2).copied().unwrap_or(0.0);

    let mut speedups: Vec<f64> = rows.iter().map(|r| r.estimated_speedup).collect();
    speedups.sort_by(f64::total_cmp);
    let median_speedup = speedups.get(speedups.len() / 2).copied().unwrap_or(1.0);

    let full_fallback_commits = rows
        .iter()
        .filter(|r| r.effective_tier == ConfidenceTier::FullVerified)
        .count();
    let zero_impact_commits = rows.iter().filter(|r| r.impacted_test_nodes == 0).count();
    let total_cochange_existing: usize = rows.iter().map(|r| r.cochange_existing_tests).sum();
    let total_cochange_predicted: usize = rows.iter().map(|r| r.cochange_predicted_tests).sum();
    let cochange_recall_pct = if total_cochange_existing == 0 {
        100.0
    } else {
        (total_cochange_predicted as f64 / total_cochange_existing as f64) * 100.0
    };

    let (median_duration_speedup, mean_duration_reduction_pct, total_suite_duration_ms) =
        if total_known_duration_ms > 0.0 {
            let mut d_speedups: Vec<f64> = rows
                .iter()
                .filter_map(|r| r.duration_weighted_speedup)
                .collect();
            d_speedups.sort_by(f64::total_cmp);
            let med = d_speedups.get(d_speedups.len() / 2).copied();
            let mean_red = d_speedups
                .iter()
                .map(|&sp| (1.0 - (1.0 / sp.max(1.0))) * 100.0)
                .sum::<f64>()
                / d_speedups.len().max(1) as f64;
            (med, Some(mean_red), Some(total_known_duration_ms))
        } else {
            (None, None, None)
        };

    Ok(ReplaySummary {
        commits_analyzed: rows.len(),
        tier,
        symbol_level_tia: graph.config.symbol_level_tia,
        has_dynamic_traces: graph.has_dynamic_traces,
        total_test_files: graph.tests_by_file.len(),
        total_test_nodes: graph.tests.len(),
        mean_impacted_files_pct,
        median_impacted_nodes_pct,
        mean_reduction_pct,
        median_speedup,
        median_duration_speedup,
        mean_duration_reduction_pct,
        total_suite_duration_ms,
        full_fallback_commits,
        zero_impact_commits,
        total_cochange_existing_tests: total_cochange_existing,
        total_cochange_predicted_tests: total_cochange_predicted,
        cochange_recall_pct,
        rows,
    })
}

/// Resolve the shared git common directory (so all git worktrees share one cache dir).
pub fn git_common_dir(repo_root: &Path) -> Option<std::path::PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let p = std::path::PathBuf::from(&raw);
    if p.is_absolute() {
        Some(p)
    } else {
        Some(repo_root.join(p))
    }
}

fn git_commit_files(repo_root: &Path, sha: &str) -> Result<(Vec<String>, BTreeSet<String>)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["diff-tree", "--no-commit-id", "--name-status", "-r", sha])
        .output()
        .with_context(|| format!("git diff-tree failed for {sha}"))?;

    let mut changed = Vec::new();
    let mut added = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next() else {
            continue;
        };
        let Some(path1) = parts.next() else {
            continue;
        };
        let p = normalize_rel_path(path1);
        if status.starts_with('A') {
            added.insert(p.clone());
        }
        changed.push(p);
        if let Some(path2) = parts.next() {
            changed.push(normalize_rel_path(path2));
        }
    }
    changed.sort();
    changed.dedup();
    Ok((changed, added))
}

fn diff_git_commit_python(
    repo_root: &Path,
    sha: &str,
    rel_path: &str,
    config: &PyrnConfig,
) -> ChangedSymbols {
    let Some(before) = git_show_bytes(repo_root, &format!("{sha}^:{rel_path}")) else {
        return ChangedSymbols::All;
    };
    let Some(after) = git_show_bytes(repo_root, &format!("{sha}:{rel_path}")) else {
        return ChangedSymbols::All;
    };
    if before == after {
        return ChangedSymbols::None;
    }
    let raw_before = blake3_domain_hex(DOMAIN_RAW_BLOB, &before);
    let raw_after = blake3_domain_hex(DOMAIN_RAW_BLOB, &after);
    let p_before = parse_python_file(rel_path, &before, &raw_before, config);
    let p_after = parse_python_file(rel_path, &after, &raw_after, config);
    diff_python_symbols(&p_before, &p_after)
}

fn git_show_bytes(repo_root: &Path, spec: &str) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show", spec])
        .output()
        .ok()?;
    if out.status.success() {
        Some(out.stdout)
    } else {
        None
    }
}
