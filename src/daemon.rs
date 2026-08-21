use anyhow::{anyhow, Result};
use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;

#[cfg(target_os = "macos")]
pub fn install_daemon(
    trace_db_bin: &Path,
    db_path: &Path,
    interval: u64,
    agents: Option<String>,
    mode: Option<String>,
    exclude: Option<String>,
    root: Option<&Path>,
) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine HOME directory"))?;
    let launch_agents_dir = home.join("Library/LaunchAgents");
    let label = "com.tracedb.watch-daemon";
    let plist_path = launch_agents_dir.join(format!("{label}.plist"));
    let log_dir = home.join(".config/trace-db");
    let log_path = log_dir.join("daemon.log");

    fs::create_dir_all(&launch_agents_dir)?;
    fs::create_dir_all(&log_dir)?;

    let mut program_args = vec![trace_db_bin.display().to_string()];
    program_args.push("watch".to_string());
    program_args.push("--interval".to_string());
    program_args.push(interval.to_string());
    program_args.push("--db".to_string());
    program_args.push(db_path.display().to_string());

    if let Some(agents) = agents {
        program_args.push("--agent".to_string());
        program_args.push(agents);
    }

    if let Some(mode) = mode {
        program_args.push("--mode".to_string());
        program_args.push(mode);
    }

    if let Some(exclude) = exclude {
        program_args.push("--exclude".to_string());
        program_args.push(exclude);
    }

    if let Some(root) = root {
        program_args.push("--root".to_string());
        program_args.push(root.display().to_string());
    }

    let plist_content = generate_plist(label, &program_args, &log_path)?;
    fs::write(&plist_path, plist_content)?;

    // Unload if already loaded
    let _ = ProcessCommand::new("launchctl")
        .args(["unload", plist_path.to_str().unwrap()])
        .output();

    // Load the new configuration
    let output = ProcessCommand::new("launchctl")
        .args(["load", plist_path.to_str().unwrap()])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to load daemon: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("✓ TraceDB watch daemon installed successfully");
    println!("  Label: {}", label);
    println!("  Plist: {}", plist_path.display());
    println!("  Log: {}", log_path.display());
    println!("  Interval: {}s", interval);
    println!("\nThe daemon will run watch every {} seconds.", interval);
    println!("Use 'trace-db daemon status' to check the status.");

    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall_daemon() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine HOME directory"))?;
    let launch_agents_dir = home.join("Library/LaunchAgents");
    let label = "com.tracedb.watch-daemon";
    let plist_path = launch_agents_dir.join(format!("{label}.plist"));

    if !plist_path.exists() {
        println!("Daemon is not installed");
        return Ok(());
    }

    let output = ProcessCommand::new("launchctl")
        .args(["unload", plist_path.to_str().unwrap()])
        .output()?;

    if !output.status.success() {
        eprintln!(
            "warning: failed to unload daemon: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fs::remove_file(&plist_path)?;

    println!("✓ TraceDB watch daemon uninstalled successfully");
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn daemon_status() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine HOME directory"))?;
    let launch_agents_dir = home.join("Library/LaunchAgents");
    let label = "com.tracedb.watch-daemon";
    let plist_path = launch_agents_dir.join(format!("{label}.plist"));

    if !plist_path.exists() {
        println!("Status: Not installed");
        return Ok(());
    }

    let output = ProcessCommand::new("launchctl")
        .args(["list", label])
        .output()?;

    if output.status.success() {
        println!("Status: Running");
        println!("\nDaemon details:");
        println!("{}", String::from_utf8_lossy(&output.stdout));
    } else {
        println!("Status: Installed but not running");
    }

    println!("\nPlist file: {}", plist_path.display());
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn start_daemon() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine HOME directory"))?;
    let launch_agents_dir = home.join("Library/LaunchAgents");
    let label = "com.tracedb.watch-daemon";
    let plist_path = launch_agents_dir.join(format!("{label}.plist"));

    if !plist_path.exists() {
        anyhow::bail!("Daemon is not installed. Run 'trace-db daemon install' first.");
    }

    let output = ProcessCommand::new("launchctl")
        .args(["start", label])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to start daemon: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("✓ TraceDB watch daemon started");
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn stop_daemon() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine HOME directory"))?;
    let label = "com.tracedb.watch-daemon";
    let plist_path = home.join("Library/LaunchAgents").join(format!("{label}.plist"));

    if !plist_path.exists() {
        anyhow::bail!("Daemon is not installed");
    }

    let output = ProcessCommand::new("launchctl")
        .args(["stop", label])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to stop daemon: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("✓ TraceDB watch daemon stopped");
    Ok(())
}

#[cfg(target_os = "macos")]
fn generate_plist(label: &str, program_args: &[String], log_path: &Path) -> Result<String> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine HOME directory"))?;

    let args_xml = program_args
        .iter()
        .map(|arg| format!("    <string>{}</string>", escape_xml(arg)))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{}</string>

  <key>ProgramArguments</key>
  <array>
{}
  </array>

  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>HOME</key>
    <string>{}</string>
  </dict>

  <key>WorkingDirectory</key>
  <string>{}</string>

  <key>StartInterval</key>
  <integer>{}</integer>

  <key>RunAtLoad</key>
  <true/>

  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>

  <key>KeepAlive</key>
  <false/>
</dict>
</plist>"#,
        label,
        args_xml,
        home.display(),
        home.display(),
        program_args
            .iter()
            .position(|arg| arg == "--interval")
            .and_then(|i| program_args.get(i + 1))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1800),
        log_path.display(),
        log_path.display()
    ))
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(not(target_os = "macos"))]
pub fn install_daemon(
    _trace_db_bin: &Path,
    _db_path: &Path,
    _interval: u64,
    _agents: Option<String>,
    _mode: Option<String>,
    _exclude: Option<String>,
    _root: Option<&Path>,
) -> Result<()> {
    anyhow::bail!("Daemon management is currently only supported on macOS")
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall_daemon() -> Result<()> {
    anyhow::bail!("Daemon management is currently only supported on macOS")
}

#[cfg(not(target_os = "macos"))]
pub fn daemon_status() -> Result<()> {
    anyhow::bail!("Daemon management is currently only supported on macOS")
}

#[cfg(not(target_os = "macos"))]
pub fn start_daemon() -> Result<()> {
    anyhow::bail!("Daemon management is currently only supported on macOS")
}

#[cfg(not(target_os = "macos"))]
pub fn stop_daemon() -> Result<()> {
    anyhow::bail!("Daemon management is currently only supported on macOS")
}

