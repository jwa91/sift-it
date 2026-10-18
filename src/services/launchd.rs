//! `sift-it service install|uninstall|status`: the background indexer and the search engine as LaunchAgents, so they
//! start when you log in and come back if they crash. Two agents, so a crash in a reader can't take search down, and
//! search runs at full priority while indexing stays in the background.

use crate::config;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// A LaunchAgent: a `sift-it` command launchd runs at login, and again if it crashes.
pub struct Agent {
    pub label: &'static str,
    command: &'static str,
    log: &'static str,
    /// Runs at a lower priority (Nice, low-priority I/O), out of the way of everything else.
    background: bool,
}

pub const INDEXER: Agent =
    Agent { label: "local.sift-it.indexer", command: "daemon", log: "indexer.log", background: true };
pub const ENGINE: Agent =
    Agent { label: "local.sift-it.engine", command: "engine", log: "engine.log", background: false };
const AGENTS: [&Agent; 2] = [&INDEXER, &ENGINE];

impl Agent {
    fn plist_path(&self) -> PathBuf {
        config::dirs_home().join(format!("Library/LaunchAgents/{}.plist", self.label))
    }

    fn service(&self) -> String {
        format!("{}/{}", domain(), self.label)
    }

    /// Whether launchd runs this agent as this binary would install it: the same binary, with the same data folder.
    pub fn installed_as_this(&self) -> bool {
        let installed = std::fs::read_to_string(self.plist_path()).ok();
        installed.is_some_and(|installed| self.plist().is_ok_and(|plist| plist == installed)) && loaded(&self.service())
    }

    /// Starts it now (it may have exited, or been stopped).
    pub fn kickstart(&self) -> Result<()> {
        let output = Command::new("launchctl").args(["kickstart", &self.service()]).output()?;
        if !output.status.success() {
            bail!("launchctl couldn't start {}: {}", self.label, String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(())
    }

    /// The LaunchAgent for this binary: runs the command at login, and again if it crashes.
    pub fn plist(&self) -> Result<String> {
        let binary = stable_binary()?;
        let log = config::home().join(self.log);
        let mut environment = String::new();
        for name in ["SIFT_IT_HOME", "SIFT_IT_MODELS"] {
            if let Ok(value) = std::env::var(name) {
                environment
                    .push_str(&format!("\n        <key>{name}</key>\n        <string>{}</string>", escape(&value)));
            }
        }
        Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>{command}</string>
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
    <integer>30</integer>{priority}
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
            escape(&binary.to_string_lossy()),
            label = self.label,
            command = self.command,
            log = escape(&log.to_string_lossy()),
            priority = if self.background {
                "\n    <key>Nice</key>\n    <integer>5</integer>\n    <key>LowPriorityIO</key>\n    <true/>"
            } else {
                "\n    <key>ProcessType</key>\n    <string>Interactive</string>"
            },
        ))
    }

    fn install(&self) -> Result<()> {
        let path = self.plist_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::create_dir_all(config::home())?;
        // A running one first goes, so the new plist (a new binary path) takes. bootout returns before it's gone (the
        // indexer finishes the file it's on), and a bootstrap meanwhile fails with "5: Input/output error": wait for
        // it.
        let service = self.service();
        let _ = Command::new("launchctl").args(["bootout", &service]).output();
        let deadline = Instant::now() + Duration::from_secs(30);
        while loaded(&service) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        std::fs::write(&path, self.plist()?)?;
        let mut tries = 0;
        loop {
            let output = Command::new("launchctl").args(["bootstrap", &domain()]).arg(&path).output()?;
            if output.status.success() {
                break;
            }
            tries += 1;
            if tries == 5 {
                bail!("launchctl couldn't start {}: {}", self.label, String::from_utf8_lossy(&output.stderr).trim());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        println!("installed {}", path.display());
        Ok(())
    }
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

/// Installs and starts both agents.
pub fn install() -> Result<()> {
    for agent in AGENTS {
        agent.install()?;
    }
    println!("the background indexer and the search engine start at login from now on");
    Ok(())
}

/// Whether launchd still has the service (`launchctl print` knows it).
fn loaded(service: &str) -> bool {
    Command::new("launchctl").args(["print", service]).output().is_ok_and(|output| output.status.success())
}

pub fn uninstall() -> Result<()> {
    for agent in AGENTS {
        let _ = Command::new("launchctl").args(["bootout", &agent.service()]).output();
        let path = agent.plist_path();
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
    }
    println!("removed the background indexer and the search engine");
    Ok(())
}

pub fn status() -> Result<()> {
    let engine = crate::clients::Client::running().is_some();
    let status: serde_json::Map<String, serde_json::Value> = AGENTS
        .iter()
        .map(|agent| {
            let running =
                if agent.label == INDEXER.label { crate::services::indexer::running_pid().is_some() } else { engine };
            let key = if agent.label == INDEXER.label { "indexer" } else { "engine" };
            let value = serde_json::json!({
                "installed": agent.plist_path().exists(),
                "loaded": loaded(&agent.service()),
                "running": running,
                "plist": agent.plist_path().display().to_string(),
            });
            (key.to_string(), value)
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

/// The plists `service install` would write.
pub fn plists() -> Result<String> {
    Ok(AGENTS.iter().map(|agent| agent.plist()).collect::<Result<Vec<_>>>()?.join("\n"))
}
