//! `sift-it service install|uninstall|status`: the background indexer as a LaunchAgent, so it starts when you log in and
//! comes back if it crashes.

use crate::config;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

pub const LABEL: &str = "local.sift-it.indexer";

fn plist_path() -> PathBuf {
    config::dirs_home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The path the LaunchAgent runs: this binary, or for a Homebrew install its link in Homebrew's bin, which stays put
/// when an upgrade replaces the versioned folder the binary itself is in.
fn stable_binary() -> Result<PathBuf> {
    let binary = std::env::current_exe().context("can't tell where sift-it is")?;
    let resolved = binary.canonicalize().unwrap_or_else(|_| binary.clone());
    let text = resolved.to_string_lossy();
    for marker in ["/Caskroom/", "/Cellar/"] {
        if let Some(position) = text.find(marker) {
            let link = PathBuf::from(&text[..position]).join("bin/sift-it");
            if link.exists() {
                return Ok(link);
            }
        }
    }
    Ok(resolved)
}

/// The LaunchAgent for this binary: runs `sift-it daemon` at login, again if it crashes, at a lower priority.
pub fn plist() -> Result<String> {
    let binary = stable_binary()?;
    let log = config::home().join("indexer.log");
    let mut environment = String::new();
    for name in ["SIFT_IT_HOME", "SIFT_IT_MODELS"] {
        if let Ok(value) = std::env::var(name) {
            environment.push_str(&format!("\n        <key>{name}</key>\n        <string>{}</string>", escape(&value)));
        }
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>daemon</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>{environment}
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>30</integer>
    <key>Nice</key>
    <integer>5</integer>
    <key>LowPriorityIO</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        escape(&binary.to_string_lossy()),
        log = escape(&log.to_string_lossy()),
    ))
}

pub fn install() -> Result<()> {
    let path = plist_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::create_dir_all(config::home())?;
    // A running one first goes, so the new plist (a new binary path) takes.
    let _ = Command::new("launchctl").args(["bootout", &format!("{}/{LABEL}", domain())]).output();
    std::fs::write(&path, plist()?)?;
    let output = Command::new("launchctl").args(["bootstrap", &domain()]).arg(&path).output()?;
    if !output.status.success() {
        bail!("launchctl couldn't start it: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    println!("installed {} and started the background indexer", path.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = Command::new("launchctl").args(["bootout", &format!("{}/{LABEL}", domain())]).output();
    let path = plist_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    println!("removed the background indexer");
    Ok(())
}

pub fn status() -> Result<()> {
    let installed = plist_path().exists();
    let output = Command::new("launchctl").args(["print", &format!("{}/{LABEL}", domain())]).output()?;
    let loaded = output.status.success();
    let pid = crate::daemon::running_pid();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "installed": installed,
            "loaded": loaded,
            "running": pid.is_some(),
            "pid": pid,
            "plist": plist_path().display().to_string(),
        }))?
    );
    Ok(())
}
