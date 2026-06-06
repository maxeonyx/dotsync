use std::process::Command;

#[test]
fn version_json_flag_prints_machine_readable_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_dotsync"))
        .args(["--version", "--json"])
        .output()
        .expect("run dotsync --version --json");

    assert!(
        output.status.success(),
        "dotsync --version --json should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
    assert_eq!(value["package"], "dotsync");
    assert_eq!(value["binary"], "dotsync");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
}
