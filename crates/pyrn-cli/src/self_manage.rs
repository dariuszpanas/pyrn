//! Installation inspection and self-update helpers (`pyrn self status|update`).

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::env;
use std::path::Path;
use std::process::Command;

const INSTALL_SH_URL: &str = "https://dariuszpanas.github.io/pyrn/install.sh";
const INSTALL_PS1_URL: &str = "https://dariuszpanas.github.io/pyrn/install.ps1";

/// Structured snapshot returned by `pyrn self status`.
#[derive(Debug, Clone, Serialize)]
pub struct SelfStatus {
    pub version: String,
    pub executable: String,
    pub target: String,
    pub install_owner: String,
    pub update_hint: String,
}

#[must_use]
pub fn current_target_triple() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-musl"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-musl"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "x86_64-pc-windows-msvc"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "aarch64-pc-windows-msvc"
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "aarch64"),
    )))]
    {
        "unknown"
    }
}

/// Detect how the current `pyrn` executable was installed.
#[must_use]
pub fn detect_install_owner(exe: &Path) -> (&'static str, String) {
    let norm = exe
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if norm.contains("/.cargo/bin/") {
        return (
            "cargo",
            "cargo install --locked --force pyrn-cli".to_owned(),
        );
    }
    if norm.contains("/uv/tools/") || norm.contains("/site-packages/pyrn/_bin/") {
        return ("uv-tool", "uv tool upgrade pyrn".to_owned());
    }
    if norm.contains("/target/debug/") || norm.contains("/target/release/") {
        return (
            "cargo-build",
            "rebuild from local checkout with `cargo build --release -p pyrn-cli`".to_owned(),
        );
    }
    if cfg!(windows) {
        ("standalone", format!("irm {INSTALL_PS1_URL} | iex"))
    } else {
        ("standalone", format!("curl -fsSL {INSTALL_SH_URL} | sh"))
    }
}

/// Collect current installation diagnostics.
pub fn inspect_self_status() -> Result<SelfStatus> {
    let exe = env::current_exe().context("resolve current pyrn executable path")?;
    let (owner, hint) = detect_install_owner(&exe);
    Ok(SelfStatus {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        executable: exe.display().to_string(),
        target: current_target_triple().to_owned(),
        install_owner: owner.to_owned(),
        update_hint: hint,
    })
}

/// Perform (or preview with `dry_run`) a self-update through the owning installation channel.
pub fn run_self_update(dry_run: bool, version: Option<&str>) -> Result<String> {
    let status = inspect_self_status()?;
    if let Some(v) = version {
        let trimmed = v.trim().trim_start_matches('v');
        if trimmed.is_empty()
            || !trimmed
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
        {
            bail!("invalid --version '{v}'");
        }
    }

    match status.install_owner.as_str() {
        "cargo-build" => {
            bail!(
                "cannot self-update a local Cargo target build ({}); rebuild from your checkout or install a standalone release",
                status.executable
            );
        }
        "cargo" => {
            let mut args = vec!["install", "--locked", "--force", "pyrn-cli"];
            let ver_owned;
            if let Some(v) = version {
                ver_owned = v.trim().trim_start_matches('v').to_owned();
                args.push("--version");
                args.push(&ver_owned);
            }
            let cmd_line = format!("cargo {}", args.join(" "));
            if dry_run {
                return Ok(format!("[dry-run] would run: {cmd_line}"));
            }
            let st = Command::new("cargo")
                .args(&args)
                .status()
                .context("invoke cargo install")?;
            if !st.success() {
                bail!("`{cmd_line}` exited with status {st}");
            }
            Ok(format!("Updated pyrn via `{cmd_line}`."))
        }
        "uv-tool" => {
            let cmd_line = match version {
                Some(v) => format!(
                    "uv tool install --force pyrn=={}",
                    v.trim().trim_start_matches('v')
                ),
                None => "uv tool upgrade pyrn".to_owned(),
            };
            if dry_run {
                return Ok(format!("[dry-run] would run: {cmd_line}"));
            }
            let st = if let Some(v) = version {
                Command::new("uv")
                    .args([
                        "tool",
                        "install",
                        "--force",
                        &format!("pyrn=={}", v.trim().trim_start_matches('v')),
                    ])
                    .status()
                    .context("invoke uv tool install")?
            } else {
                Command::new("uv")
                    .args(["tool", "upgrade", "pyrn"])
                    .status()
                    .context("invoke uv tool upgrade")?
            };
            if !st.success() {
                bail!("`{cmd_line}` exited with status {st}");
            }
            Ok(format!("Updated pyrn via `{cmd_line}`."))
        }
        _ => {
            let ver_tag = version.map(|v| {
                let clean = v.trim().trim_start_matches('v');
                format!("v{clean}")
            });
            if cfg!(windows) {
                let ps_cmd = match ver_tag {
                    Some(ref tag) => {
                        format!("$env:PYRN_VERSION='{tag}'; irm {INSTALL_PS1_URL} | iex")
                    }
                    None => format!("irm {INSTALL_PS1_URL} | iex"),
                };
                if dry_run {
                    return Ok(format!(
                        "[dry-run] would run PowerShell installer: {ps_cmd}"
                    ));
                }
                let mut cmd = Command::new("powershell");
                cmd.args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    &ps_cmd,
                ]);
                let st = cmd
                    .status()
                    .context("invoke PowerShell standalone installer")?;
                if !st.success() {
                    bail!("PowerShell standalone installer exited with status {st}");
                }
                Ok(
                    "Updated standalone pyrn installation via verified release installer."
                        .to_owned(),
                )
            } else {
                let sh_cmd = format!("curl -fsSL {INSTALL_SH_URL} | sh");
                if dry_run {
                    return Ok(format!(
                        "[dry-run] would run standalone installer: {}{sh_cmd}",
                        ver_tag
                            .as_ref()
                            .map_or_else(String::new, |t| format!("PYRN_VERSION={t} "))
                    ));
                }
                let mut cmd = Command::new("sh");
                cmd.args(["-c", &sh_cmd]);
                if let Some(ref tag) = ver_tag {
                    cmd.env("PYRN_VERSION", tag);
                }
                let st = cmd.status().context("invoke standalone shell installer")?;
                if !st.success() {
                    bail!("standalone installer exited with status {st}");
                }
                Ok(
                    "Updated standalone pyrn installation via verified release installer."
                        .to_owned(),
                )
            }
        }
    }
}
