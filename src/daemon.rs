#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::anyhow;
use anyhow::Result;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;

fn watch_args(
    trace_db_bin: &Path,
    db_path: &Path,
    interval: u64,
    agents: Option<String>,
    mode: Option<String>,
    exclude: Option<String>,
    root: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        trace_db_bin.display().to_string(),
        "watch".into(),
        "--interval".into(),
        interval.to_string(),
        "--db".into(),
        db_path.display().to_string(),
    ];
    if let Some(value) = agents {
        args.extend(["--agent".into(), value]);
    }
    if let Some(value) = mode {
        args.extend(["--mode".into(), value]);
    }
    if let Some(value) = exclude {
        args.extend(["--exclude".into(), value]);
    }
    if let Some(value) = root {
        args.extend(["--root".into(), value.display().to_string()]);
    }
    args
}

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

    let program_args = watch_args(trace_db_bin, db_path, interval, agents, mode, exclude, root);

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
    let plist_path = home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));

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

#[cfg(target_os = "macos")]
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "linux")]
pub fn install_daemon(
    trace_db_bin: &Path,
    db_path: &Path,
    interval: u64,
    agents: Option<String>,
    mode: Option<String>,
    exclude: Option<String>,
    root: Option<&Path>,
) -> Result<()> {
    let config =
        dirs::config_dir().ok_or_else(|| anyhow!("cannot determine configuration directory"))?;
    let unit_dir = config.join("systemd/user");
    let unit_path = unit_dir.join("tracedb-watch.service");
    fs::create_dir_all(&unit_dir)?;
    let args = watch_args(trace_db_bin, db_path, interval, agents, mode, exclude, root);
    fs::write(&unit_path, generate_systemd_unit(&args))?;
    run_systemctl(["--user", "daemon-reload"])?;
    run_systemctl(["--user", "enable", "--now", "tracedb-watch.service"])?;
    println!("TraceDB watch daemon installed successfully");
    println!("  Unit: {}", unit_path.display());
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall_daemon() -> Result<()> {
    let unit_path = user_unit_path()?;
    if !unit_path.exists() {
        println!("Daemon is not installed");
        return Ok(());
    }
    run_systemctl(["--user", "disable", "--now", "tracedb-watch.service"])?;
    fs::remove_file(unit_path)?;
    run_systemctl(["--user", "daemon-reload"])?;
    println!("TraceDB watch daemon uninstalled successfully");
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn daemon_status() -> Result<()> {
    let unit_path = user_unit_path()?;
    if !unit_path.exists() {
        println!("Status: Not installed");
        return Ok(());
    }
    let output = ProcessCommand::new("systemctl")
        .args(["--user", "is-active", "tracedb-watch.service"])
        .output()?;
    println!(
        "Status: {}",
        if output.status.success() {
            "Running"
        } else {
            "Installed but not running"
        }
    );
    println!("Unit file: {}", unit_path.display());
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn start_daemon() -> Result<()> {
    ensure_linux_unit()?;
    run_systemctl(["--user", "start", "tracedb-watch.service"])?;
    println!("TraceDB watch daemon started");
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn stop_daemon() -> Result<()> {
    ensure_linux_unit()?;
    run_systemctl(["--user", "stop", "tracedb-watch.service"])?;
    println!("TraceDB watch daemon stopped");
    Ok(())
}

#[cfg(target_os = "linux")]
fn user_unit_path() -> Result<std::path::PathBuf> {
    Ok(dirs::config_dir()
        .ok_or_else(|| anyhow!("cannot determine configuration directory"))?
        .join("systemd/user/tracedb-watch.service"))
}

#[cfg(target_os = "linux")]
fn ensure_linux_unit() -> Result<()> {
    if !user_unit_path()?.exists() {
        anyhow::bail!("Daemon is not installed. Run 'trace-db daemon install' first.");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_systemctl<const N: usize>(args: [&str; N]) -> Result<()> {
    let output = ProcessCommand::new("systemctl").args(args).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "systemctl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn generate_systemd_unit(args: &[String]) -> String {
    let command = args
        .iter()
        .map(|arg| systemd_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!("[Unit]\nDescription=TraceDB native-session watcher\nAfter=default.target\n\n[Service]\nExecStart={command}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n")
}

#[cfg(target_os = "linux")]
fn systemd_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-:=".contains(c))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(target_os = "windows")]
pub fn install_daemon(
    trace_db_bin: &Path,
    db_path: &Path,
    interval: u64,
    agents: Option<String>,
    mode: Option<String>,
    exclude: Option<String>,
    root: Option<&Path>,
) -> Result<()> {
    let task = windows_task_command(trace_db_bin, db_path, interval, agents, mode, exclude, root);
    let output = ProcessCommand::new("schtasks")
        .args([
            "/Create",
            "/TN",
            "TraceDB-Watch",
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
            "/TR",
            &task,
        ])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "schtasks failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    println!("TraceDB watch daemon installed successfully (Task Scheduler: TraceDB-Watch)");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn uninstall_daemon() -> Result<()> {
    run_schtasks(["/Delete", "/TN", "TraceDB-Watch", "/F"])?;
    println!("TraceDB watch daemon uninstalled successfully");
    Ok(())
}
#[cfg(target_os = "windows")]
pub fn daemon_status() -> Result<()> {
    run_schtasks(["/Query", "/TN", "TraceDB-Watch"])?;
    Ok(())
}
#[cfg(target_os = "windows")]
pub fn start_daemon() -> Result<()> {
    run_schtasks(["/Run", "/TN", "TraceDB-Watch"])?;
    println!("TraceDB watch daemon started");
    Ok(())
}
#[cfg(target_os = "windows")]
pub fn stop_daemon() -> Result<()> {
    run_schtasks(["/End", "/TN", "TraceDB-Watch"])?;
    println!("TraceDB watch daemon stopped");
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_schtasks<const N: usize>(args: [&str; N]) -> Result<()> {
    let output = ProcessCommand::new("schtasks").args(args).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "schtasks failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_task_command(
    trace_db_bin: &Path,
    db_path: &Path,
    interval: u64,
    agents: Option<String>,
    mode: Option<String>,
    exclude: Option<String>,
    root: Option<&Path>,
) -> String {
    watch_args(trace_db_bin, db_path, interval, agents, mode, exclude, root)
        .iter()
        .map(|arg| windows_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(target_os = "windows")]
fn windows_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
pub fn install_daemon(
    _trace_db_bin: &Path,
    _db_path: &Path,
    _interval: u64,
    _agents: Option<String>,
    _mode: Option<String>,
    _exclude: Option<String>,
    _root: Option<&Path>,
) -> Result<()> {
    anyhow::bail!("Daemon management is not supported on this platform")
}
#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
pub fn uninstall_daemon() -> Result<()> {
    anyhow::bail!("Daemon management is not supported on this platform")
}
#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
pub fn daemon_status() -> Result<()> {
    anyhow::bail!("Daemon management is not supported on this platform")
}
#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
pub fn start_daemon() -> Result<()> {
    anyhow::bail!("Daemon management is not supported on this platform")
}
#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
pub fn stop_daemon() -> Result<()> {
    anyhow::bail!("Daemon management is not supported on this platform")
}
