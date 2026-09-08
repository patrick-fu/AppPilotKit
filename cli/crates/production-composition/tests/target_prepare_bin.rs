use std::io::Write;
use std::process::{Command, Stdio};

fn target_prepare_bin() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_apppilotkit-target-prepare").into()
}

#[test]
fn target_prepare_rejects_invalid_arguments_with_exit_code_2() {
    let bin = target_prepare_bin();

    // No arguments
    let output = Command::new(&bin).output().expect("run");
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json error");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["kind"], "cli.invalidInvocation");

    // Only --output=json
    let output = Command::new(&bin)
        .arg("--output=json")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));

    // Unknown flag
    let output = Command::new(&bin)
        .arg("--unknown-flag")
        .arg("--output=json")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));

    // --release-fd=0 without --output=json
    let output = Command::new(&bin)
        .arg("--release-fd=0")
        .arg("--output=text")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));

    // --request-fd=0 without --output=json
    let output = Command::new(&bin)
        .arg("--request-fd=0")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn target_prepare_release_mode_rejects_malformed_input_with_exit_code_1() {
    let bin = target_prepare_bin();

    let mut child = Command::new(&bin)
        .arg("--release-fd=0")
        .arg("--output=json")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"not json")
        .expect("write");

    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json error");
    assert_eq!(json["schema_version"], "1.0");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["kind"], "cli.invalidInvocation");
    assert_eq!(json["error"]["stage"], "input");
}

#[test]
fn target_prepare_release_mode_rejects_oversize_input_with_exit_code_1() {
    let bin = target_prepare_bin();

    let mut child = Command::new(&bin)
        .arg("--release-fd=0")
        .arg("--output=json")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");

    let oversize = vec![b' '; 65_537];
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(&oversize)
        .expect("write");

    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json error");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["kind"], "cli.invalidInvocation");
}

#[test]
fn target_prepare_request_mode_remains_compatible() {
    let bin = target_prepare_bin();

    let mut child = Command::new(&bin)
        .arg("--request-fd=0")
        .arg("--output=json")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"{\"bad\":\"input\"}")
        .expect("write");

    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json error");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["kind"], "cli.invalidInvocation");
}
