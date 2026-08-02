use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const FIXTURE_BINARY: &str = env!("CARGO_BIN_EXE_apppilotkit-cli-contract-fixture");

#[test]
fn discovery_is_self_contained_with_an_empty_and_poisoned_environment() {
    for command in [
        vec!["capabilities", "--output", "json"],
        vec!["schema", "list", "--output", "json"],
        vec![
            "schema",
            "show",
            "https://apppilotkit.dev/cli/v1/machine-result.schema.json",
            "--output",
            "json",
        ],
        vec!["doctor", "--output", "json", "--non-interactive"],
    ] {
        let output = isolated_command(&command)
            .output()
            .expect("fixture CLI runs");
        assert_successful_machine_output(&output);
    }
}

#[test]
fn discovery_ignores_null_and_redirected_stdin() {
    let null_output = isolated_command(&["capabilities", "--output", "json"])
        .stdin(Stdio::null())
        .output()
        .expect("fixture CLI runs with null stdin");
    assert_successful_machine_output(&null_output);

    let mut child = isolated_command(&["capabilities", "--output", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fixture CLI starts with redirected stdin");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"input that discovery must ignore\n")
        .expect("test input writes");
    let redirected_output = child.wait_with_output().expect("fixture CLI exits");
    assert_successful_machine_output(&redirected_output);
}

#[test]
fn process_channels_and_exit_status_match_the_contract() {
    let help = isolated_command(&["--help"]).output().expect("help runs");
    assert!(help.status.success());
    assert!(!help.stdout.is_empty());
    assert!(help.stderr.is_empty());

    let usage = isolated_command(&["unknown", "--output", "json"])
        .output()
        .expect("usage failure runs");
    assert_eq!(usage.status.code(), Some(2));
    assert!(usage.stderr.is_empty());
    let result: Value = serde_json::from_slice(&usage.stdout).expect("bare machine JSON stdout");
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error"]["kind"], "cli.invalidInvocation");
    assert_eq!(result["command"], serde_json::json!([]));

    let diagnostic = isolated_command(&["unknown"])
        .output()
        .expect("parser diagnostic runs");
    assert_eq!(diagnostic.status.code(), Some(2));
    assert!(diagnostic.stdout.is_empty());
    assert!(!diagnostic.stderr.is_empty());
    assert!(serde_json::from_slice::<Value>(&diagnostic.stderr).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn discovery_succeeds_when_the_operating_system_denies_network_access() {
    let sandbox = Path::new("/usr/bin/sandbox-exec");
    if !sandbox.exists() {
        eprintln!("sandbox-exec is unavailable; macOS network-denial probe skipped");
        return;
    }
    let output = Command::new(sandbox)
        .env_clear()
        .args([
            "-p",
            "(version 1) (allow default) (deny network*)",
            FIXTURE_BINARY,
            "capabilities",
            "--output",
            "json",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("sandboxed fixture CLI runs");
    assert_successful_machine_output(&output);
}

fn isolated_command(arguments: &[&str]) -> Command {
    let mut command = Command::new(FIXTURE_BINARY);
    command
        .env_clear()
        .env("HOME", "/poisoned/home")
        .env("PATH", "/poisoned/tools")
        .env("APPPILOTKIT_XCRUN", "/poisoned/xcrun")
        .env("APPPILOTKIT_ADB", "/poisoned/adb")
        .env("APPPILOTKIT_CREDENTIALS", "/poisoned/credentials")
        .current_dir(std::env::temp_dir())
        .args(arguments)
        .stdin(Stdio::null());
    command
}

fn assert_successful_machine_output(output: &Output) {
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let result: Value = serde_json::from_slice(&output.stdout).expect("bare machine JSON stdout");
    assert_eq!(result["status"], "succeeded");
    assert_eq!(result["side_effect"], "read_only");
    assert_eq!(result["retry_safety"], "safe");
}
