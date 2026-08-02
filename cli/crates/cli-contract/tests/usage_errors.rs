use apppilotkit_cli_contract::{CliConfig, CliCore, ProcessOutput};
use serde_json::Value;

#[test]
fn one_unambiguous_machine_selector_keeps_usage_failures_on_stdout() {
    let core = core();

    let missing_id = core.run(["fixture-cli", "schema", "show", "--output", "json"]);
    assert_machine_usage_failure(missing_id, &["schema", "show"]);

    let unknown = core.run(["fixture-cli", "unknown", "--output=json"]);
    assert_machine_usage_failure(unknown, &[]);
}

#[test]
fn ambiguous_or_invalid_output_selection_uses_only_a_parser_diagnostic() {
    let core = core();
    for argv in [
        vec!["fixture-cli", "unknown"],
        vec!["fixture-cli", "unknown", "--output", "yaml"],
        vec![
            "fixture-cli",
            "unknown",
            "--output",
            "json",
            "--output",
            "json",
        ],
        vec!["fixture-cli", "unknown", "--output=json", "--output=jsonl"],
        vec!["fixture-cli", "schema", "show", "--", "--output", "json"],
    ] {
        let output = core.run(argv);
        assert_eq!(output.exit_code, 2);
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn jsonl_usage_failure_has_only_one_failed_terminal_event() {
    let output = core().run(["fixture-cli", "schema", "nope", "--output", "jsonl"]);

    assert_eq!(output.exit_code, 2);
    assert!(output.stderr.is_empty());
    let events = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("JSONL event"))
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "run.failed");
    assert_eq!(
        events[0]["result"]["command"],
        serde_json::json!(["schema"])
    );
    assert_eq!(
        events[0]["result"]["error"]["kind"],
        "cli.invalidInvocation"
    );
}

#[test]
fn tokens_after_the_option_terminator_are_not_recognized_as_commands() {
    let output = core().run(["fixture-cli", "schema", "--output", "json", "--", "show"]);

    assert_machine_usage_failure(output, &["schema"]);
}

#[test]
fn unknown_schema_is_a_structured_changed_request_not_a_panic() {
    let output = core().run([
        "fixture-cli",
        "schema",
        "show",
        "https://example.invalid/not-installed.schema.json",
        "--output",
        "json",
    ]);

    assert_eq!(output.exit_code, 2);
    assert!(output.stderr.is_empty());
    let result: Value = serde_json::from_slice(&output.stdout).expect("failure Machine Result");
    assert_eq!(result["status"], "failed");
    assert_eq!(result["command"], serde_json::json!(["schema", "show"]));
    assert_eq!(result["error"]["kind"], "cli.invalidInvocation");
    assert!(
        !output
            .stdout
            .windows(b"example.invalid".len())
            .any(|window| window == b"example.invalid")
    );
}

fn core() -> CliCore {
    CliCore::new(CliConfig::new("fixture-cli", "0.1.0"))
        .expect("embedded contract should initialize")
}

fn assert_machine_usage_failure(output: ProcessOutput, expected_command: &[&str]) {
    assert_eq!(output.exit_code, 2);
    assert!(output.stderr.is_empty());
    let result: Value = serde_json::from_slice(&output.stdout).expect("failure Machine Result");
    assert_eq!(result["status"], "failed");
    assert_eq!(result["command"], serde_json::json!(expected_command));
    assert_eq!(result["side_effect"], "read_only");
    assert_eq!(result["retry_safety"], "safe");
    assert_eq!(result["error"]["kind"], "cli.invalidInvocation");
    assert_eq!(result["error"]["retryable"], false);
}
