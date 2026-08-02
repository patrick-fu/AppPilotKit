use apppilotkit_cli_contract::{CliConfig, CliCore};
use serde_json::Value;

#[test]
fn agent_can_discover_the_complete_installed_contract_as_one_machine_result() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0"))
        .expect("embedded contract should initialize");

    let output = core.run(["fixture-cli", "capabilities", "--output", "json"]);

    assert_eq!(output.exit_code, 0);
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    let result: Value = serde_json::from_slice(&output.stdout).expect("machine result JSON");
    assert_eq!(result["schema_version"], "1.0");
    assert_eq!(result["cli_version"], "0.1.0");
    assert_eq!(result["status"], "succeeded");
    assert_eq!(result["command"], serde_json::json!(["capabilities"]));
    assert_eq!(result["side_effect"], "read_only");
    assert_eq!(result["retry_safety"], "safe");
    assert_eq!(result["data"]["executable"], "fixture-cli");
    assert_eq!(
        result["data"]["output_modes"],
        serde_json::json!(["human", "json", "jsonl"])
    );
    assert_eq!(
        result["data"]["commands"]
            .as_array()
            .expect("commands array")
            .iter()
            .map(|command| command["path"].clone())
            .collect::<Vec<_>>(),
        [
            serde_json::json!([]),
            serde_json::json!(["capabilities"]),
            serde_json::json!(["doctor"]),
            serde_json::json!(["schema"]),
            serde_json::json!(["schema", "list"]),
            serde_json::json!(["schema", "show"]),
        ]
    );
    for command in result["data"]["commands"]
        .as_array()
        .expect("commands array")
    {
        assert!(
            command["aliases"].is_array(),
            "command aliases are explicit"
        );
    }
    assert_eq!(
        result["data"]["commands"]
            .as_array()
            .expect("commands array")
            .iter()
            .find(|command| command["path"] == serde_json::json!(["schema", "show"]))
            .expect("schema show command")["arguments"],
        serde_json::json!([{
            "id": "schema-id",
            "long": null,
            "short": null,
            "aliases": [],
            "value_name": "SCHEMA_ID",
            "values": [],
            "required": true,
            "help": "Installed schema identifier"
        }])
    );
    let error_kinds = result["data"]["error_kinds"]
        .as_array()
        .expect("error kind catalog");
    assert!(
        error_kinds
            .iter()
            .any(|entry| { entry["kind"] == "cli.internalError" && entry["exit_code"] == 1 })
    );
    assert!(
        error_kinds.iter().any(|entry| {
            entry["kind"] == "target.selectionRequired" && entry["exit_code"] == 4
        })
    );
    assert_eq!(
        result["data"]["global_arguments"],
        serde_json::json!([
            {
                "id": "help", "long": "help", "short": "h", "aliases": [],
                "value_name": null, "values": [], "required": false,
                "help": "Print help for the current command"
            },
            {
                "id": "non-interactive", "long": "non-interactive", "short": null,
                "aliases": [], "value_name": null, "values": [], "required": false,
                "help": "Never prompt or consume implicit input"
            },
            {
                "id": "output", "long": "output", "short": null, "aliases": [],
                "value_name": "MODE", "values": ["human", "json", "jsonl"],
                "required": false,
                "help": "Select deterministic human, JSON, or JSONL output"
            }
        ])
    );
    let command_fingerprint = result["data"]["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .map(|command| {
            serde_json::json!({
                "path": command["path"],
                "aliases": command["aliases"],
                "arguments": command["arguments"],
                "result_schema_id": command["result_schema_id"],
                "result_fields": command["result_fields"],
                "error_kinds": command["error_kinds"],
                "side_effect": command["side_effect"],
                "retry_safety": command["retry_safety"],
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        command_fingerprint,
        serde_json::json!([
            {
                "path": [], "aliases": [],
                "arguments": [{
                    "id": "version", "long": "version", "short": "V", "aliases": [],
                    "value_name": null, "values": [], "required": false,
                    "help": "Print the CLI version"
                }],
                "result_schema_id": null, "result_fields": [],
                "error_kinds": ["cli.invalidInvocation"],
                "side_effect": "read_only", "retry_safety": "safe"
            },
            {
                "path": ["capabilities"], "aliases": [], "arguments": [],
                "result_schema_id": "https://apppilotkit.dev/cli/v1/capability-manifest.schema.json",
                "result_fields": ["cli_version", "commands", "error_kinds", "executable", "global_arguments", "output_modes", "retry_safety_values", "schema_version", "schemas", "side_effect_classes"],
                "error_kinds": ["cli.internalError"],
                "side_effect": "read_only", "retry_safety": "safe"
            },
            {
                "path": ["doctor"], "aliases": [], "arguments": [],
                "result_schema_id": "https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/doctorReport",
                "result_fields": ["checks"], "error_kinds": ["cli.internalError"],
                "side_effect": "read_only", "retry_safety": "safe"
            },
            {
                "path": ["schema"], "aliases": [], "arguments": [],
                "result_schema_id": null, "result_fields": [],
                "error_kinds": ["cli.invalidInvocation"],
                "side_effect": "read_only", "retry_safety": "safe"
            },
            {
                "path": ["schema", "list"], "aliases": [], "arguments": [],
                "result_schema_id": "https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/schemaList",
                "result_fields": ["schemas"], "error_kinds": ["cli.internalError"],
                "side_effect": "read_only", "retry_safety": "safe"
            },
            {
                "path": ["schema", "show"], "aliases": [],
                "arguments": [{
                    "id": "schema-id", "long": null, "short": null, "aliases": [],
                    "value_name": "SCHEMA_ID", "values": [], "required": true,
                    "help": "Installed schema identifier"
                }],
                "result_schema_id": "https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/schemaShow",
                "result_fields": ["schema", "schema_id"],
                "error_kinds": ["cli.internalError", "cli.invalidInvocation"],
                "side_effect": "read_only", "retry_safety": "safe"
            }
        ])
        .as_array()
        .unwrap()
        .clone()
    );
    assert!(result["artifacts"].as_array().is_some_and(Vec::is_empty));
    assert!(result["next_actions"].as_array().is_some());
}
