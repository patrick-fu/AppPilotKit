use apppilotkit_rust_foundation_spike::{command_manifest, run_cli};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn document_and_jsonl_outputs_are_newline_framed_and_keep_diagnostics_separate() {
    let document = run_cli([
        "spike",
        "emit",
        "--format",
        "document",
        "--summary",
        "登录 ready",
    ]);
    assert_eq!(document.exit_code, 0);
    assert!(document.stderr.is_empty());
    assert_eq!(document.stdout.last(), Some(&b'\n'));
    assert_eq!(
        document
            .stdout
            .iter()
            .filter(|byte| **byte == b'\n')
            .count(),
        1
    );
    let parsed: Value = serde_json::from_slice(&document.stdout).expect("document should be JSON");
    assert_eq!(parsed["summary"], "登录 ready");

    let jsonl = run_cli(["spike", "emit", "--format", "jsonl", "--summary", "ready"]);
    assert_eq!(jsonl.exit_code, 0);
    assert!(jsonl.stderr.is_empty());
    assert_eq!(jsonl.stdout.last(), Some(&b'\n'));
    let events = jsonl
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("each line should be JSON"))
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["terminal"] == true)
            .count(),
        1
    );
    assert_eq!(
        events.last().expect("terminal event")["type"],
        "run.succeeded"
    );

    let invalid = run_cli(["spike", "emit", "--format", "invalid"]);
    assert_ne!(invalid.exit_code, 0);
    assert!(invalid.stdout.is_empty());
    assert!(!invalid.stderr.is_empty());
}

#[test]
fn jsonl_terminal_event_and_process_exit_status_describe_the_same_outcome() {
    for (outcome, terminal_type, exit_code) in [
        ("succeeded", "run.succeeded", 0),
        ("failed", "run.failed", 1),
        ("cancelled", "run.cancelled", 130),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_apppilotkit-rust-foundation-spike"))
            .args(["emit", "--format", "jsonl", "--outcome", outcome])
            .stdin(Stdio::null())
            .output()
            .expect("outcome process should run");
        assert_eq!(output.status.code(), Some(exit_code));
        assert!(output.stderr.is_empty());
        let events = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).expect("JSONL event"))
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["terminal"] == true)
                .count(),
            1
        );
        let terminal = events.last().expect("terminal event");
        assert_eq!(terminal["type"], terminal_type);
        assert_eq!(terminal["result"]["outcome"], outcome);
    }
}

#[test]
fn help_and_version_are_successful_stdout_responses() {
    for argument in ["--help", "--version"] {
        let library = run_cli(["spike", argument]);
        assert_eq!(library.exit_code, 0, "{argument} should succeed");
        assert!(library.stderr.is_empty());
        assert_eq!(library.stdout.last(), Some(&b'\n'));

        let binary = Command::new(env!("CARGO_BIN_EXE_apppilotkit-rust-foundation-spike"))
            .arg(argument)
            .stdin(Stdio::null())
            .output()
            .expect("help or version process should run");
        assert!(binary.status.success(), "{argument} should succeed");
        assert!(binary.stderr.is_empty());
        assert_eq!(binary.stdout.last(), Some(&b'\n'));
    }
}

#[test]
fn manifest_contains_each_public_command_and_argument_once() {
    let manifest = command_manifest();
    assert_complete_manifest(&manifest);
}

fn assert_complete_manifest(manifest: &Value) {
    let commands = manifest["commands"].as_array().expect("commands array");
    let paths = commands
        .iter()
        .map(|command| command["path"].as_str().expect("command path"))
        .collect::<Vec<_>>();
    assert_eq!(paths, ["spike", "spike emit", "spike manifest"]);

    let argument_records = commands
        .iter()
        .flat_map(|command| {
            let path = command["path"].as_str().expect("command path");
            command["arguments"]
                .as_array()
                .expect("arguments array")
                .iter()
                .map(move |argument| {
                    (
                        format!("{path}:{}", argument["id"].as_str().expect("argument id")),
                        argument["long"].as_str().map(str::to_owned),
                        argument["short"].as_str().map(str::to_owned),
                        argument["aliases"]
                            .as_array()
                            .expect("aliases array")
                            .iter()
                            .map(|alias| alias.as_str().expect("alias").to_owned())
                            .collect::<Vec<_>>(),
                        argument["required"].as_bool().expect("required flag"),
                    )
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        argument_records,
        [
            ("spike:help", Some("help"), Some("h")),
            ("spike:version", Some("version"), Some("V")),
            ("spike emit:format", Some("format"), Some("f")),
            ("spike emit:help", Some("help"), Some("h")),
            ("spike emit:outcome", Some("outcome"), None),
            ("spike emit:summary", Some("summary"), None),
            ("spike manifest:help", Some("help"), Some("h")),
        ]
        .map(|(key, long, short)| (
            key.to_owned(),
            long.map(str::to_owned),
            short.map(str::to_owned),
            Vec::new(),
            false,
        ))
    );
}

#[test]
fn manifest_black_box_needs_no_stdin_or_environment() {
    let output = Command::new(env!("CARGO_BIN_EXE_apppilotkit-rust-foundation-spike"))
        .arg("manifest")
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .expect("manifest process should run");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("manifest should be JSON");
    assert_complete_manifest(&parsed);
}

#[cfg(target_os = "macos")]
#[test]
fn manifest_black_box_succeeds_with_network_denied_and_device_paths_poisoned() {
    let output = Command::new("/usr/bin/sandbox-exec")
        .args([
            "-p",
            "(version 1)(allow default)(deny network*)",
            env!("CARGO_BIN_EXE_apppilotkit-rust-foundation-spike"),
            "manifest",
        ])
        .env_clear()
        .env("HOME", "/nonexistent")
        .env("PATH", "/nonexistent")
        .env("APPPILOTKIT_XCRUN", "/nonexistent/xcrun")
        .env("APPPILOTKIT_ADB", "/nonexistent/adb")
        .stdin(Stdio::null())
        .output()
        .expect("sandboxed manifest process should run");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("manifest should be JSON");
    assert_complete_manifest(&parsed);
}

#[test]
fn redirected_stdin_never_triggers_a_prompt_or_contaminates_output() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_apppilotkit-rust-foundation-spike"))
        .args(["emit", "--format", "document", "--summary", "piped"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("emit process should start");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(br#"{"unexpected":"input"}"#)
        .expect("redirected input should write");
    let output = child.wait_with_output().expect("emit process should exit");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("stdout JSON");
    assert_eq!(parsed["summary"], "piped");
    assert!(parsed.get("unexpected").is_none());
}
