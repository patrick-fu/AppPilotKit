use apppilotkit_rust_foundation_spike::{command_manifest, run_cli};
use serde_json::Value;
use std::collections::HashSet;
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
fn manifest_contains_each_public_command_and_argument_once() {
    let manifest = command_manifest();
    let commands = manifest["commands"].as_array().expect("commands array");
    let paths = commands
        .iter()
        .map(|command| command["path"].as_str().expect("command path"))
        .collect::<Vec<_>>();
    assert!(paths.contains(&"spike"));
    assert!(paths.contains(&"spike emit"));
    assert!(paths.contains(&"spike manifest"));
    assert_eq!(
        paths.iter().copied().collect::<HashSet<_>>().len(),
        paths.len()
    );

    let argument_keys = commands
        .iter()
        .flat_map(|command| {
            let path = command["path"].as_str().expect("command path");
            command["arguments"]
                .as_array()
                .expect("arguments array")
                .iter()
                .map(move |argument| {
                    format!("{path}:{}", argument["id"].as_str().expect("argument id"))
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        argument_keys.iter().collect::<HashSet<_>>().len(),
        argument_keys.len()
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
    assert!(
        parsed["commands"]
            .as_array()
            .is_some_and(|commands| !commands.is_empty())
    );
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
