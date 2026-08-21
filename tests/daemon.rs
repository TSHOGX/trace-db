#[cfg(target_os = "macos")]
use std::process::Command;
#[cfg(target_os = "macos")]
use tempfile::tempdir;

#[test]
#[cfg(target_os = "macos")]
fn daemon_install_and_uninstall() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    // Install daemon
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "daemon",
            "install",
            "--interval",
            "3600",
        ])
        .env_remove("TRACEDB_CONFIG")
        .env_remove("TRACEDB_PATH")
        .output()
        .unwrap();

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        panic!("daemon install failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("installed successfully"));
    assert!(stdout.contains("com.tracedb.watch-daemon"));

    // Check status
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["daemon", "status"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Running") || stdout.contains("Installed"),
        "status output: {}",
        stdout
    );

    // Uninstall daemon
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["daemon", "uninstall"])
        .output()
        .unwrap();

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        panic!("daemon uninstall failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("uninstalled successfully"));
}

#[test]
#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
fn daemon_unsupported_platform() {
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["daemon", "install"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not supported on this platform"));
}
