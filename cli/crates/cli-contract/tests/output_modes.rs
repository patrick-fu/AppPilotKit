use apppilotkit_cli_contract::{CliConfig, CliCore};
use serde_json::Value;

#[test]
fn one_capabilities_outcome_has_deterministic_human_json_and_jsonl_renderings() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0"))
        .expect("embedded contract should initialize");

    let human = core.run(["fixture-cli", "capabilities"]);
    assert_eq!(human.exit_code, 0);
    assert!(human.stderr.is_empty());
    assert_eq!(
        String::from_utf8(human.stdout).expect("UTF-8 human output"),
        "Installed CLI contract 1.0 for fixture-cli 0.1.0: 6 commands, 8 schemas.\n"
    );

    let document = core.run(["fixture-cli", "capabilities", "--output=json"]);
    assert_eq!(document.exit_code, 0);
    assert!(document.stderr.is_empty());
    assert_eq!(
        document
            .stdout
            .iter()
            .filter(|byte| **byte == b'\n')
            .count(),
        1
    );
    let document: Value = serde_json::from_slice(&document.stdout).expect("single Machine Result");
    assert_eq!(document["status"], "succeeded");

    let jsonl = core.run(["fixture-cli", "capabilities", "--output", "jsonl"]);
    assert_eq!(jsonl.exit_code, 0);
    assert!(jsonl.stderr.is_empty());
    let events = jsonl
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("JSONL event"))
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["type"], "run.started");
    assert_eq!(events[0]["command"], serde_json::json!(["capabilities"]));
    assert_eq!(events[0]["side_effect"], "read_only");
    assert_eq!(events[0]["retry_safety"], "safe");
    assert_eq!(events[1]["type"], "run.succeeded");
    assert_eq!(events[1]["result"], document);
    assert_eq!(events[0]["run_id"], events[1]["run_id"]);
    assert!(
        events[0]["run_id"]
            .as_str()
            .is_some_and(|run_id| run_id.starts_with("run-"))
    );
}
