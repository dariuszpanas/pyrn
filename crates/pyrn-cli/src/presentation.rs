//! Rich terminal presentation for `pyrn` powered by `fast-rich`.
//!
//! Renders rounded Unicode panels, aligned tables, styled verdict badges, and structured help
//! pages while respecting `NO_COLOR`, `FORCE_COLOR`, `GITHUB_ACTIONS`, `--plain`, and TTY state.

use crate::self_manage::SelfStatus;
use clap::Command;
use fast_rich::{
    Padding, Panel, Style, Text,
    console::RenderContext,
    padding::PaddingSpec,
    panel::BorderStyle,
    renderable::Renderable,
    style::Color,
    table::{Cell, Column, ColumnWidth, Row, Table},
    text::{Alignment, Overflow, Span},
};
use pyrn_core::{CacheStats, ExecVerdict, PyrnConfig, ReplaySummary, SlotRole, TestPlan};
use pyrn_runner::{DoctorReport, RunReport, TestOutcome, TestRecord};
use std::collections::BTreeMap;
use std::env;
use std::io::IsTerminal;
use std::path::Path;

/// Styled Rich output renderer targeting either stdout or stderr.
pub struct RichOutput {
    context: RenderContext,
    styled: bool,
    colors: bool,
    border: BorderStyle,
    buffer: String,
}

fn env_flag(name: &str) -> bool {
    env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

impl RichOutput {
    /// Create a renderer for stdout or stderr, honoring `--plain`, `NO_COLOR`, and terminal width.
    #[must_use]
    pub fn new(stderr: bool, plain: bool) -> Self {
        let terminal = if stderr {
            std::io::stderr().is_terminal()
        } else {
            std::io::stdout().is_terminal()
        };
        let width = env::var("TERMINAL_WIDTH")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .or_else(|| {
                env::var("COLUMNS")
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
            })
            .unwrap_or(96)
            .clamp(60, 140);
        let dumb = env::var("TERM")
            .is_ok_and(|s| matches!(s.to_ascii_lowercase().as_str(), "dumb" | "unknown"));
        let forced =
            env_flag("GITHUB_ACTIONS") || env_flag("FORCE_COLOR") || env_flag("CLICOLOR_FORCE");
        let styled = !plain && !dumb && (terminal || forced);
        let colors = styled && !env_flag("NO_COLOR");
        Self {
            context: RenderContext {
                width,
                height: None,
            },
            styled,
            colors,
            border: if plain {
                BorderStyle::Ascii
            } else {
                BorderStyle::Rounded
            },
            buffer: String::new(),
        }
    }

    #[must_use]
    pub fn finish(self) -> String {
        self.buffer
    }

    fn append(&mut self, item: &impl Renderable) {
        for segment in item.render(&self.context) {
            for span in segment.spans {
                let prefix = if self.styled {
                    sgr(&span.style, self.colors)
                } else {
                    String::new()
                };
                if !prefix.is_empty() && !span.text.is_empty() {
                    self.buffer.push_str("\x1b[");
                    self.buffer.push_str(&prefix);
                    self.buffer.push('m');
                    self.buffer.push_str(&span.text);
                    self.buffer.push_str("\x1b[0m");
                } else {
                    self.buffer.push_str(&span.text);
                }
            }
            if segment.newline {
                self.buffer.push('\n');
            }
        }
    }

    fn panel(
        &self,
        title: &str,
        item: impl Renderable + Send + Sync + 'static,
        style: Style,
    ) -> Panel {
        Panel::renderable(item)
            .title(title)
            .title_align(Alignment::Left)
            .title_style(style)
            .border_style(self.border)
            .style(style)
            .expand(true)
            .padding(1, 0)
    }

    fn kv_table(rows: Vec<(&'static str, Text)>) -> Table {
        let mut table = Table::new()
            .show_header(false)
            .show_border(false)
            .show_edge(false)
            .show_row_lines(false)
            .pad_edge(false)
            .padding(1)
            .expand(true);
        table.add_column(
            Column::new("")
                .width(ColumnWidth::Fixed(18))
                .no_wrap(true)
                .style(Style::new().dim()),
        );
        table.add_column(Column::new("").overflow(Overflow::Wrap));
        for (k, v) in rows {
            table.add_row_obj(Row::cells([Cell::Text(Text::from(k)), Cell::Text(v)]));
        }
        table
    }

    /// Render a framed error box to stderr.
    pub fn render_error(&mut self, title: &str, message: &str) {
        let text = Text::from(message.to_owned());
        self.append(&self.panel(title, text, Style::new().foreground(Color::Red).bold()));
    }

    /// Render Rich command help matching framed Options / Commands panels.
    pub fn render_help(&mut self, cmd: &Command) {
        let mut usage_cmd = cmd.clone();
        let usage = usage_cmd.render_usage().to_string();
        let mut usage_text = Text::new();
        usage_text.push_styled(usage, Style::new().bold());
        self.append(&Padding::new(usage_text, PaddingSpec::new(1, 1, 0, 1)));

        if let Some(about) = cmd.get_about() {
            self.append(&Padding::new(
                Text::from(about.to_string()),
                PaddingSpec::new(1, 1, 1, 1),
            ));
        }

        // Options panel
        let mut opt_rows: Vec<(String, String)> = Vec::new();
        for arg in cmd.get_arguments() {
            let id = arg.get_id().as_str();
            if arg.is_hide_set() || arg.is_positional() || matches!(id, "help" | "version") {
                continue;
            }
            let mut flag = String::new();
            if let Some(short) = arg.get_short() {
                flag.push('-');
                flag.push(short);
                if arg.get_long().is_some() {
                    flag.push_str(", ");
                }
            }
            if let Some(long) = arg.get_long() {
                flag.push_str("--");
                flag.push_str(long);
            }
            if arg.get_action().takes_values() {
                let vn = arg
                    .get_value_names()
                    .and_then(|v| v.first())
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_else(|| arg.get_id().as_str().to_ascii_uppercase());
                flag.push(' ');
                flag.push_str(&vn);
            }
            let help = arg.get_help().map(ToString::to_string).unwrap_or_default();
            opt_rows.push((flag, help));
        }
        opt_rows.push((
            "-h, --help".to_owned(),
            "Show this message and exit.".to_owned(),
        ));
        if cmd.get_version().is_some() {
            opt_rows.push((
                "-V, --version".to_owned(),
                "Show the installed pyrn version and exit.".to_owned(),
            ));
        }

        if !opt_rows.is_empty() {
            let max_flag = opt_rows
                .iter()
                .map(|(f, _)| f.len())
                .max()
                .unwrap_or(16)
                .clamp(14, 34);
            let mut table = Table::new()
                .show_header(false)
                .show_border(false)
                .show_edge(false)
                .show_row_lines(false)
                .pad_edge(false)
                .padding(1)
                .expand(true);
            table.add_column(
                Column::new("")
                    .width(ColumnWidth::Fixed(max_flag + 1))
                    .no_wrap(true)
                    .style(Style::parse("bold cyan")),
            );
            table.add_column(Column::new("").overflow(Overflow::Wrap));
            for (f, h) in opt_rows {
                table.add_row_obj(Row::cells([
                    Cell::Text(Text::from(f)),
                    Cell::Text(Text::from(h)),
                ]));
            }
            self.append(&self.panel("Options", table, Style::new().dim()));
        }

        // Commands panel
        let subcommands: Vec<&Command> = cmd
            .get_subcommands()
            .filter(|s| !s.is_hide_set() && s.get_name() != "help")
            .collect();
        if !subcommands.is_empty() {
            let max_name = subcommands
                .iter()
                .map(|s| s.get_name().len())
                .max()
                .unwrap_or(12)
                .clamp(12, 20);
            let mut table = Table::new()
                .show_header(false)
                .show_border(false)
                .show_edge(false)
                .show_row_lines(false)
                .pad_edge(false)
                .padding(1)
                .expand(true);
            table.add_column(
                Column::new("")
                    .width(ColumnWidth::Fixed(max_name + 2))
                    .no_wrap(true)
                    .style(Style::parse("bold yellow")),
            );
            table.add_column(Column::new("").overflow(Overflow::Wrap));
            for sc in subcommands {
                let about = sc.get_about().map(ToString::to_string).unwrap_or_default();
                table.add_row_obj(Row::cells([
                    Cell::Text(Text::from(sc.get_name().to_owned())),
                    Cell::Text(Text::from(about)),
                ]));
            }
            self.append(&self.panel("Commands", table, Style::new().dim()));
        }

        let mut footer = Text::new();
        footer.push_styled("Docs: ", Style::new().dim());
        footer.push_styled(
            "https://dariuszpanas.github.io/pyrn/commands.html",
            Style::parse("cyan"),
        );
        self.append(&Padding::new(footer, PaddingSpec::new(0, 1, 1, 1)));
    }

    /// Render a Rich summary + diagnostics report for `pyrn run`.
    pub fn render_run_report(
        &mut self,
        report: &RunReport,
        cache_seeded: usize,
        top_slow: usize,
        show_failures: usize,
    ) {
        let (verdict_label, border_style) = match report.verdict {
            ExecVerdict::Pass => ("PASSED", Style::new().foreground(Color::Green).bold()),
            ExecVerdict::Flaky => ("FLAKY", Style::new().foreground(Color::Yellow).bold()),
            ExecVerdict::Failed => ("FAILED", Style::new().foreground(Color::Red).bold()),
            ExecVerdict::TimedOut => ("TIMED OUT", Style::new().foreground(Color::Red).bold()),
            ExecVerdict::ConfigError => ("ERROR", Style::new().foreground(Color::Red).bold()),
        };

        let fp_pct = if report.total_collected > 0 {
            (report.fast_path_tests as f64 / report.total_collected as f64) * 100.0
        } else {
            0.0
        };

        let mut outcome_text = Text::new();
        outcome_text.push_styled(
            format!("{} passed", report.passed),
            Style::new().foreground(Color::Green).bold(),
        );
        if report.tests_cached > 0 {
            outcome_text.push_styled(
                format!(" · {} cached", report.tests_cached),
                Style::new().foreground(Color::Cyan).bold(),
            );
        }
        let total_fail = report.failed + report.errors + report.timed_out + report.crashed;
        if total_fail > 0 {
            outcome_text.push_styled(
                format!(
                    " · {} failed ({} err, {} timeout, {} crash)",
                    report.failed, report.errors, report.timed_out, report.crashed
                ),
                Style::new().foreground(Color::Red).bold(),
            );
        } else {
            outcome_text.push_styled(" · 0 failed", Style::new().dim());
        }
        if report.skipped > 0 {
            outcome_text.push_styled(
                format!(" · {} skipped", report.skipped),
                Style::new().foreground(Color::Yellow),
            );
        }
        if report.flaky > 0 {
            outcome_text.push_styled(
                format!(" · {} flaky", report.flaky),
                Style::new().foreground(Color::Yellow).bold(),
            );
        }
        outcome_text.push_styled(
            format!(
                "  (executed={}, deselected={})",
                report.total_collected, report.deselected
            ),
            Style::new().dim(),
        );

        let mut timing_text = Text::new();
        timing_text.push_styled(
            format!("{:.2}s wall", report.wall_ms as f64 / 1000.0),
            Style::new().bold(),
        );
        timing_text.push_styled(
            format!(
                "  |  discovery {}ms · pool {}ms · collect {:.2}s · db {:.2}s (tpl={}) · fast-path {}/{} ({:.1}%) · restarts {}",
                report.discovery_ms,
                report.pool_ready_ms,
                report.collect_total_ms / 1000.0,
                report.db_setup_total_ms / 1000.0,
                report.db_template_hits,
                report.fast_path_tests,
                report.total_collected,
                fp_pct,
                report.worker_restarts,
            ),
            Style::new().dim(),
        );

        let mut rows: Vec<(&str, Text)> = vec![
            (
                "Verdict",
                Text::from_spans([
                    Span::styled(verdict_label, border_style),
                    Span::styled(
                        format!(
                            "  ({} · role={} · tier={} · workers={} [{}])",
                            report.mode,
                            report.role.as_str(),
                            report.tier.as_str(),
                            report.workers_requested,
                            report.worker_limit_reason,
                        ),
                        Style::new().dim(),
                    ),
                ]),
            ),
            ("Outcomes", outcome_text),
            ("Timing", timing_text),
        ];

        if let Some(ref cov) = report.coverage_file {
            rows.push((
                "Coverage",
                Text::from_spans([Span::styled(
                    format!("sysmon merged -> {cov}"),
                    Style::new().foreground(Color::Cyan),
                )]),
            ));
        }
        if cache_seeded > 0 {
            rows.push((
                "CAS Cache",
                Text::from_spans([Span::styled(
                    format!("seeded {cache_seeded} passing entries"),
                    Style::new().foreground(Color::Cyan),
                )]),
            ));
        }

        let summary_table = Self::kv_table(rows);
        let title = format!("pyrn run · {}", report.role.as_str());
        self.append(&self.panel(&title, summary_table, border_style));

        // Failures / Faults / Flakes panel
        let failures: Vec<&TestRecord> = report
            .results
            .iter()
            .filter(|r| r.outcome.is_failure() || r.outcome == TestOutcome::Flaky)
            .collect();
        if !failures.is_empty() && show_failures > 0 {
            let mut body = Text::new();
            for (idx, rec) in failures.iter().take(show_failures).enumerate() {
                if idx > 0 {
                    body.push("\n");
                }
                let badge_style = if rec.outcome == TestOutcome::Flaky {
                    Style::new().foreground(Color::Yellow).bold()
                } else {
                    Style::new().foreground(Color::Red).bold()
                };
                body.push_styled(format!("[{:?}] ", rec.outcome), badge_style);
                body.push_styled(rec.nodeid.clone(), Style::new().bold());
                body.push_styled(
                    format!(
                        " ({:.1}ms, {}, attempts={})\n",
                        rec.total_ms, rec.worker_id, rec.attempts
                    ),
                    Style::new().dim(),
                );
                if let Some(ref lr) = rec.longrepr {
                    let lines: Vec<&str> = lr.lines().collect();
                    let start = lines.len().saturating_sub(14);
                    if start > 0 {
                        body.push_styled(
                            format!("  ... ({start} earlier lines omitted)\n"),
                            Style::new().dim(),
                        );
                    }
                    for line in &lines[start..] {
                        let style = if line.trim_start().starts_with("E ") {
                            Style::new().foreground(Color::Red)
                        } else {
                            Style::new().dim()
                        };
                        body.push_styled(format!("  {line}\n"), style);
                    }
                }
            }
            let ftitle = format!("Failures / Faults / Flakes ({})", failures.len());
            self.append(&self.panel(&ftitle, body, Style::new().foreground(Color::Red).bold()));
        }

        // Collection errors panel
        if !report.collection_errors.is_empty() {
            let mut body = Text::new();
            for err in report.collection_errors.iter().take(5) {
                for line in err.lines().take(8) {
                    body.push(format!("{line}\n"));
                }
            }
            let ctitle = format!("Collection Errors ({})", report.collection_errors.len());
            self.append(&self.panel(&ctitle, body, Style::new().foreground(Color::Red).bold()));
        }

        // Slow diagnostics panel
        if top_slow > 0 && !report.results.is_empty() {
            self.render_slow_diagnostics(report, top_slow);
        }
    }

    fn render_slow_diagnostics(&mut self, report: &RunReport, top_n: usize) {
        let total_setup_ms: f64 = report.results.iter().map(|r| r.setup_ms).sum();
        let total_call_ms: f64 = report.results.iter().map(|r| r.call_ms).sum();
        let total_teardown_ms: f64 = report.results.iter().map(|r| r.teardown_ms).sum();
        let total_cpu_ms = total_setup_ms + total_call_ms + total_teardown_ms;

        let active_workers: Vec<f64> = report
            .worker_stats
            .iter()
            .filter(|w| w.units_run > 0 || w.tests_run > 0)
            .map(|w| w.wall_active_ms)
            .collect();

        let (median_w, max_w, straggler_id) = if !active_workers.is_empty() {
            let mut sorted = active_workers.clone();
            sorted.sort_by(f64::total_cmp);
            let med = sorted[sorted.len() / 2];
            let max_stat = report
                .worker_stats
                .iter()
                .max_by(|a, b| a.wall_active_ms.total_cmp(&b.wall_active_ms));
            (
                med,
                max_stat.map_or(0.0, |s| s.wall_active_ms),
                max_stat.map_or("-", |s| s.worker_id.as_str()),
            )
        } else {
            (0.0, 0.0, "-")
        };
        let parallel_eff = if report.wall_ms > 0 && !active_workers.is_empty() {
            let sum_active: f64 = active_workers.iter().sum();
            (sum_active / (report.wall_ms as f64 * active_workers.len() as f64)) * 100.0
        } else {
            0.0
        };

        let mut diag_rows = vec![
            (
                "CPU breakdown",
                Text::from(format!(
                    "setup {:.2}s ({:.1}%) · call {:.2}s ({:.1}%) · teardown {:.2}s ({:.1}%)",
                    total_setup_ms / 1000.0,
                    pct(total_setup_ms, total_cpu_ms),
                    total_call_ms / 1000.0,
                    pct(total_call_ms, total_cpu_ms),
                    total_teardown_ms / 1000.0,
                    pct(total_teardown_ms, total_cpu_ms),
                )),
            ),
            (
                "Worker balance",
                Text::from(format!(
                    "active={} · median {:.2}s · max {:.2}s ({}) · tail_skew {:.2}s · efficiency {:.1}%",
                    active_workers.len(),
                    median_w / 1000.0,
                    max_w / 1000.0,
                    straggler_id,
                    (max_w - median_w).max(0.0) / 1000.0,
                    parallel_eff,
                )),
            ),
        ];

        let mut by_file: BTreeMap<String, (f64, usize)> = BTreeMap::new();
        for r in report
            .results
            .iter()
            .filter(|r| r.outcome != TestOutcome::Cached)
        {
            let f = r.nodeid.split("::").next().unwrap_or(&r.nodeid).to_owned();
            let entry = by_file.entry(f).or_insert((0.0, 0));
            entry.0 += r.total_ms;
            entry.1 += 1;
        }
        let mut file_vec: Vec<(String, f64, usize)> =
            by_file.into_iter().map(|(f, (ms, n))| (f, ms, n)).collect();
        file_vec.sort_by(|a, b| b.1.total_cmp(&a.1));

        let mut by_test: Vec<&TestRecord> = report
            .results
            .iter()
            .filter(|r| r.outcome != TestOutcome::Cached)
            .collect();
        by_test.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));

        if !by_test.is_empty() {
            let mut top_tests = Text::new();
            for (idx, rec) in by_test.iter().take(top_n.min(5)).enumerate() {
                if idx > 0 {
                    top_tests.push("\n");
                }
                top_tests.push_styled(
                    format!("{:7.1}ms ", rec.total_ms),
                    Style::parse("bold yellow"),
                );
                top_tests.push_styled(
                    format!("[{}] ", if rec.fast_path { "fast" } else { "pytest" }),
                    Style::new().dim(),
                );
                top_tests.push(rec.nodeid.clone());
            }
            diag_rows.push(("Slowest tests", top_tests));
        }

        if !file_vec.is_empty() {
            let mut top_files = Text::new();
            for (idx, (f, ms, n)) in file_vec.iter().take(top_n.min(5)).enumerate() {
                if idx > 0 {
                    top_files.push("\n");
                }
                top_files.push_styled(format!("{:7.1}ms ", ms), Style::parse("bold cyan"));
                top_files.push_styled(format!("({n:>4} tests) "), Style::new().dim());
                top_files.push(f.clone());
            }
            diag_rows.push(("Slowest files", top_files));
        }

        self.append(&self.panel("Diagnostics", Self::kv_table(diag_rows), Style::new().dim()));
    }

    /// Render `pyrn plan` output.
    pub fn render_plan_report(&mut self, plan: &TestPlan, role: SlotRole, all_files: bool) {
        let imp = &plan.impact;
        let mut rows: Vec<(&str, Text)> = vec![
            (
                "Mode",
                Text::from(format!(
                    "pure in-memory TIA · tier={} · role={}",
                    imp.effective_tier.as_str(),
                    role.as_str()
                )),
            ),
            (
                "Selected",
                Text::from_spans([
                    Span::styled(
                        format!("{}/{} files", imp.impacted_test_files, imp.total_test_files),
                        Style::new().foreground(Color::Green).bold(),
                    ),
                    Span::styled(
                        format!(
                            " ({}/{} nodes · -{:.1}% reduction · {} symbol-pruned · {} tainted)",
                            imp.impacted_test_nodes,
                            imp.total_test_nodes,
                            imp.reduction_pct,
                            imp.symbol_pruned_edges,
                            imp.tainted_test_nodes,
                        ),
                        Style::new().dim(),
                    ),
                ]),
            ),
            (
                "Timing",
                Text::from(format!(
                    "parse {:.1}ms · graph {:.1}ms · impact {:.2}ms · env={} · proj={}",
                    plan.parse_and_hash_ms,
                    plan.graph_build_ms,
                    imp.eval_ms,
                    &plan.env_fingerprint[..16.min(plan.env_fingerprint.len())],
                    &plan.project_fingerprint[..16.min(plan.project_fingerprint.len())],
                )),
            ),
        ];
        if !imp.full_fallback_reasons.is_empty() {
            rows.push((
                "Fallback",
                Text::from_spans([Span::styled(
                    imp.full_fallback_reasons.join("; "),
                    Style::new().foreground(Color::Yellow),
                )]),
            ));
        }
        let limit = if all_files {
            plan.selected_files.len()
        } else {
            30
        };
        if !plan.selected_files.is_empty() {
            let mut file_text = Text::new();
            for (idx, f) in plan.selected_files.iter().take(limit).enumerate() {
                if idx > 0 {
                    file_text.push("\n");
                }
                file_text.push(f.clone());
            }
            if plan.selected_files.len() > limit {
                file_text.push_styled(
                    format!(
                        "\n... (+{} more; pass --all-files or --json)",
                        plan.selected_files.len() - limit
                    ),
                    Style::new().dim(),
                );
            }
            rows.push(("Files", file_text));
        }
        self.append(&self.panel(
            "pyrn plan",
            Self::kv_table(rows),
            Style::new().foreground(Color::Yellow).bold(),
        ));
    }

    /// Render `pyrn doctor` output.
    pub fn render_doctor_report(
        &mut self,
        report: &DoctorReport,
        top_slow_files: &[(String, f64)],
    ) {
        let daemon_text = if report.daemon_running {
            Text::from_spans([Span::styled(
                format!(
                    "RUNNING (pid={:?}, workers={:?})",
                    report.daemon_pid, report.daemon_workers
                ),
                Style::new().foreground(Color::Green).bold(),
            )])
        } else {
            Text::from_spans([Span::styled(
                "stopped (start with `pyrn daemon start --repo .`)",
                Style::new().dim(),
            )])
        };
        let sysmon_style = if report.sys_monitoring_available {
            Style::new().foreground(Color::Green).bold()
        } else {
            Style::new().foreground(Color::Yellow)
        };
        let mut rows = vec![
            ("Repository", Text::from(report.project_root.clone())),
            (
                "Worker budget",
                Text::from(report.worker_budget_explanation.clone()),
            ),
            (
                "Python env",
                Text::from_spans([
                    Span::styled(report.python_exe.clone(), Style::new().bold()),
                    Span::styled(
                        format!(
                            " (Python={}, pytest={}, coverage={}, sys.monitoring=",
                            report.python_version.as_deref().unwrap_or("unknown"),
                            report.pytest_version.as_deref().unwrap_or("missing"),
                            report
                                .coverage_version
                                .as_deref()
                                .unwrap_or("not installed"),
                        ),
                        Style::new().dim(),
                    ),
                    Span::styled(report.sys_monitoring_available.to_string(), sysmon_style),
                    Span::styled(")", Style::new().dim()),
                ]),
            ),
            (
                "Discovery",
                Text::from(format!(
                    "testpaths={:?} · python_files={:?} · pure_autouse_guards={:?}",
                    report.testpaths, report.python_files, report.pure_autouse_guards
                )),
            ),
            (
                "Cache / DB",
                Text::from(format!(
                    "role={} · dir={} · sqlite_template_ready={}",
                    report.role.as_str(),
                    report.cache_dir,
                    report.sqlite_template_exists,
                )),
            ),
            ("Warm daemon", daemon_text),
        ];
        if !top_slow_files.is_empty() {
            let mut slow = Text::new();
            for (idx, (file, ms)) in top_slow_files.iter().enumerate() {
                if idx > 0 {
                    slow.push("\n");
                }
                slow.push_styled(format!("{ms:8.1} ms  "), Style::parse("bold cyan"));
                slow.push(file.clone());
            }
            rows.push(("Slowest history", slow));
        }
        self.append(&self.panel(
            "pyrn doctor",
            Self::kv_table(rows),
            Style::new().foreground(Color::Cyan).bold(),
        ));
    }

    /// Render `pyrn cache status` output.
    pub fn render_cache_status(
        &mut self,
        role: SlotRole,
        db_path: &Path,
        db_bytes: u64,
        stats: &CacheStats,
    ) {
        let rows = vec![
            ("Role", Text::from(role.as_str())),
            ("DB path", Text::from(db_path.display().to_string())),
            (
                "DB size",
                Text::from(format!("{:.1} KiB", db_bytes as f64 / 1024.0)),
            ),
            (
                "Entries",
                Text::from(format!(
                    "{} total ({} agent, {} verify) · {} passed, {} failed, {} skipped",
                    stats.total_entries,
                    stats.agent_entries,
                    stats.verify_entries,
                    stats.passed_entries,
                    stats.failed_entries,
                    stats.skipped_entries
                )),
            ),
            (
                "Cache hits",
                Text::from(format!(
                    "{} cumulative hits across {} project(s)",
                    stats.total_hits, stats.distinct_projects
                )),
            ),
        ];
        self.append(&self.panel(
            "pyrn cache status",
            Self::kv_table(rows),
            Style::new().foreground(Color::Cyan).bold(),
        ));
    }

    /// Render `pyrn config show` output.
    pub fn render_config_show(&mut self, project_root: &Path, source_file: &str, cfg: &PyrnConfig) {
        let rows = vec![
            ("Repository", Text::from(project_root.display().to_string())),
            ("Source", Text::from(source_file.to_owned())),
            ("testpaths", Text::from(format!("{:?}", cfg.testpaths))),
            (
                "python_files",
                Text::from(format!("{:?}", cfg.python_files)),
            ),
            (
                "source_roots",
                Text::from(format!("{:?}", cfg.source_roots)),
            ),
            (
                "pure_autouse",
                Text::from(format!("{:?}", cfg.pure_autouse_guards)),
            ),
            (
                "max_workers",
                Text::from(
                    cfg.max_workers
                        .map_or_else(|| "auto".to_owned(), |w| w.to_string()),
                ),
            ),
            (
                "worker_mem_mb",
                Text::from(
                    cfg.worker_mem_mb
                        .map_or_else(|| "300 (default)".to_owned(), |m| format!("{m} MiB")),
                ),
            ),
            (
                "TIA policies",
                Text::from(format!(
                    "symbol_level_tia={} · conftest={:?} · unknown_path={:?}",
                    cfg.symbol_level_tia, cfg.conftest_policy, cfg.unknown_path_policy
                )),
            ),
            (
                "env_allowlist",
                Text::from(format!("{:?}", cfg.env_allowlist_prefixes)),
            ),
        ];
        self.append(&self.panel(
            "pyrn config",
            Self::kv_table(rows),
            Style::new().foreground(Color::Yellow).bold(),
        ));
    }

    /// Render `pyrn self status` output.
    pub fn render_self_status(&mut self, status: &SelfStatus) {
        let rows = vec![
            (
                "Version",
                Text::from_spans([Span::styled(status.version.clone(), Style::new().bold())]),
            ),
            ("Executable", Text::from(status.executable.clone())),
            ("Target", Text::from(status.target.clone())),
            ("Install owner", Text::from(status.install_owner.clone())),
            ("Update command", Text::from(status.update_hint.clone())),
        ];
        self.append(&self.panel(
            "pyrn self status",
            Self::kv_table(rows),
            Style::new().foreground(Color::Cyan).bold(),
        ));
    }

    /// Render `pyrn replay-git` output.
    pub fn render_replay_git(&mut self, summary: &ReplaySummary) {
        let mut rows = vec![
            (
                "Scope",
                Text::from(format!(
                    "{} commits · tier={} · symbol_tia={}",
                    summary.commits_analyzed,
                    summary.tier.as_str(),
                    summary.symbol_level_tia
                )),
            ),
            (
                "Co-change recall",
                Text::from(format!(
                    "{}/{} ({:.1}%)",
                    summary.total_cochange_predicted_tests,
                    summary.total_cochange_existing_tests,
                    summary.cochange_recall_pct
                )),
            ),
            (
                "Node speedup",
                Text::from(format!(
                    "{:.2}x median ({:.1}% mean reduction)",
                    summary.median_speedup, summary.mean_reduction_pct
                )),
            ),
        ];
        if let Some(dur_sp) = summary.median_duration_speedup {
            rows.push((
                "Duration speedup",
                Text::from(format!(
                    "{:.2}x median ({:.1}% mean wall saved)",
                    dur_sp,
                    summary.mean_duration_reduction_pct.unwrap_or(0.0)
                )),
            ));
        }
        rows.push((
            "Fallbacks",
            Text::from(format!(
                "{} full · {} zero-impact",
                summary.full_fallback_commits, summary.zero_impact_commits
            )),
        ));
        self.append(&self.panel(
            "pyrn replay-git",
            Self::kv_table(rows),
            Style::new().foreground(Color::Yellow).bold(),
        ));
        for row in &summary.rows {
            self.buffer.push_str(&format!(
                "  {} {:>4}/{:<4} files ({:>5.1}% red, {:>3} sym-pruned) [{}] {}\n",
                row.sha,
                row.impacted_test_files,
                row.total_test_files,
                row.reduction_pct,
                row.symbol_pruned_edges,
                row.effective_tier.as_str(),
                row.subject,
            ));
        }
    }
}

fn pct(part: f64, total: f64) -> f64 {
    if total > 0.0 {
        (part / total) * 100.0
    } else {
        0.0
    }
}

fn sgr(style: &Style, colors: bool) -> String {
    let mut bits: Vec<String> = Vec::new();
    for (on, code) in [
        (style.bold, "1"),
        (style.dim, "2"),
        (style.italic, "3"),
        (style.underline, "4"),
        (style.reverse, "7"),
    ] {
        if on {
            bits.push(code.to_owned());
        }
    }
    if colors && let Some(c) = style.foreground {
        match c {
            Color::Black => bits.push("30".to_owned()),
            Color::Red => bits.push("31".to_owned()),
            Color::Green => bits.push("32".to_owned()),
            Color::Yellow => bits.push("33".to_owned()),
            Color::Blue => bits.push("34".to_owned()),
            Color::Magenta => bits.push("35".to_owned()),
            Color::Cyan => bits.push("36".to_owned()),
            Color::White => bits.push("37".to_owned()),
            Color::Rgb { r, g, b } => bits.push(format!("38;2;{r};{g};{b}")),
            _ => {}
        }
    }
    bits.join(";")
}
