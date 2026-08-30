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
            serde_json::json!(["catalog"]),
            serde_json::json!(["catalog", "invoke"]),
            serde_json::json!(["catalog", "list"]),
            serde_json::json!(["catalog", "query"]),
            serde_json::json!(["catalog", "schema"]),
            serde_json::json!(["catalog", "show"]),
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
    assert!(
        error_kinds.iter().any(|entry| {
            entry["kind"] == "session.selectionRequired" && entry["exit_code"] == 4
        })
    );
    assert!(error_kinds.iter().any(|entry| {
        entry["kind"] == "semantic.capabilityNotFound" && entry["exit_code"] == 4
    }));
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
        serde_json::json!(
[
    {
        "path": [],
        "aliases": [],
        "arguments": [
            {
                "aliases": [],
                "help": "Print the CLI version",
                "id": "version",
                "long": "version",
                "required": false,
                "short": "V",
                "value_name": null,
                "values": []
            }
        ],
        "result_schema_id": null,
        "result_fields": [],
        "error_kinds": [
            "cli.invalidInvocation"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "capabilities"
        ],
        "aliases": [],
        "arguments": [],
        "result_schema_id": "https://apppilotkit.dev/cli/v1/capability-manifest.schema.json",
        "result_fields": [
            "cli_version",
            "commands",
            "error_kinds",
            "executable",
            "global_arguments",
            "output_modes",
            "retry_safety_values",
            "schema_version",
            "schemas",
            "side_effect_classes"
        ],
        "error_kinds": [
            "cli.internalError"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "catalog"
        ],
        "aliases": [],
        "arguments": [],
        "result_schema_id": null,
        "result_fields": [],
        "error_kinds": [
            "cli.invalidInvocation"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "catalog",
            "invoke"
        ],
        "aliases": [],
        "arguments": [
            {
                "aliases": [],
                "help": "One-time destructive authorization grant for this invoke only",
                "id": "authorization-grant",
                "long": "authorization-grant",
                "required": false,
                "short": null,
                "value_name": "GRANT",
                "values": []
            },
            {
                "aliases": [],
                "help": "Registered Semantic Capability identifier",
                "id": "capability",
                "long": "capability",
                "required": true,
                "short": null,
                "value_name": "CAPABILITY",
                "values": []
            },
            {
                "aliases": [],
                "help": "Declaration revision for the selected capability",
                "id": "declaration-revision",
                "long": "declaration-revision",
                "required": true,
                "short": null,
                "value_name": "REVISION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Bounded UTF-8 JSON value for the capability input",
                "id": "input",
                "long": "input",
                "required": true,
                "short": null,
                "value_name": "JSON",
                "values": []
            },
            {
                "aliases": [],
                "help": "Input schema handle sha256 digest",
                "id": "input-schema-digest",
                "long": "input-schema-digest",
                "required": true,
                "short": null,
                "value_name": "DIGEST",
                "values": []
            },
            {
                "aliases": [],
                "help": "Input schema handle identifier",
                "id": "input-schema-id",
                "long": "input-schema-id",
                "required": true,
                "short": null,
                "value_name": "SCHEMA",
                "values": []
            },
            {
                "aliases": [],
                "help": "Input schema handle revision",
                "id": "input-schema-revision",
                "long": "input-schema-revision",
                "required": true,
                "short": null,
                "value_name": "REVISION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Opened Protocol Session identifier",
                "id": "session",
                "long": "session",
                "required": false,
                "short": null,
                "value_name": "SESSION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Explicit Target identifier",
                "id": "target",
                "long": "target",
                "required": false,
                "short": null,
                "value_name": "TARGET",
                "values": []
            }
        ],
        "result_schema_id": "https://apppilotkit.dev/cli/v1/catalog.schema.json#/$defs/invoke",
        "result_fields": [
            "capability",
            "completed",
            "declaration_revision"
        ],
        "error_kinds": [
            "action.conflict",
            "action.outcomeUnknown",
            "action.policyDenied",
            "capabilityUnavailable",
            "cli.internalError",
            "cli.invalidInvocation",
            "cursorExpired",
            "incompatibleProtocol",
            "internalError",
            "invalidParams",
            "invalidRequest",
            "methodNotFound",
            "parseError",
            "resourceExhausted",
            "semantic.capabilityNotFound",
            "semantic.disclosureDenied",
            "semantic.schemaMismatch",
            "semantic.unavailable",
            "session.selectionRequired",
            "sessionExpired",
            "target.selectionRequired",
            "timeout",
            "transport.authenticationRequired"
        ],
        "side_effect": "app_mutation",
        "retry_safety": "requires_idempotency_key"
    },
    {
        "path": [
            "catalog",
            "list"
        ],
        "aliases": [],
        "arguments": [
            {
                "aliases": [],
                "help": "Opaque catalog list continuation cursor",
                "id": "cursor",
                "long": "cursor",
                "required": false,
                "short": null,
                "value_name": "CURSOR",
                "values": []
            },
            {
                "aliases": [],
                "help": "Requested maximum response bytes",
                "id": "max-bytes",
                "long": "max-bytes",
                "required": false,
                "short": null,
                "value_name": "BYTES",
                "values": []
            },
            {
                "aliases": [],
                "help": "Requested maximum catalog items",
                "id": "max-items",
                "long": "max-items",
                "required": false,
                "short": null,
                "value_name": "COUNT",
                "values": []
            },
            {
                "aliases": [],
                "help": "Opened Protocol Session identifier",
                "id": "session",
                "long": "session",
                "required": false,
                "short": null,
                "value_name": "SESSION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Explicit Target identifier",
                "id": "target",
                "long": "target",
                "required": false,
                "short": null,
                "value_name": "TARGET",
                "values": []
            }
        ],
        "result_schema_id": "https://apppilotkit.dev/cli/v1/catalog.schema.json#/$defs/list",
        "result_fields": [
            "capabilities",
            "catalog"
        ],
        "error_kinds": [
            "capabilityUnavailable",
            "cli.internalError",
            "cli.invalidInvocation",
            "cursorExpired",
            "incompatibleProtocol",
            "internalError",
            "invalidParams",
            "invalidRequest",
            "methodNotFound",
            "parseError",
            "resourceExhausted",
            "semantic.capabilityNotFound",
            "semantic.disclosureDenied",
            "semantic.schemaMismatch",
            "semantic.unavailable",
            "session.selectionRequired",
            "sessionExpired",
            "target.selectionRequired",
            "timeout",
            "transport.authenticationRequired"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "catalog",
            "query"
        ],
        "aliases": [],
        "arguments": [
            {
                "aliases": [],
                "help": "Registered Semantic Capability identifier",
                "id": "capability",
                "long": "capability",
                "required": true,
                "short": null,
                "value_name": "CAPABILITY",
                "values": []
            },
            {
                "aliases": [],
                "help": "Declaration revision for the selected capability",
                "id": "declaration-revision",
                "long": "declaration-revision",
                "required": true,
                "short": null,
                "value_name": "REVISION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Bounded UTF-8 JSON value for the capability input",
                "id": "input",
                "long": "input",
                "required": false,
                "short": null,
                "value_name": "JSON",
                "values": []
            },
            {
                "aliases": [],
                "help": "Input schema handle sha256 digest",
                "id": "input-schema-digest",
                "long": "input-schema-digest",
                "required": false,
                "short": null,
                "value_name": "DIGEST",
                "values": []
            },
            {
                "aliases": [],
                "help": "Input schema handle identifier",
                "id": "input-schema-id",
                "long": "input-schema-id",
                "required": false,
                "short": null,
                "value_name": "SCHEMA",
                "values": []
            },
            {
                "aliases": [],
                "help": "Input schema handle revision",
                "id": "input-schema-revision",
                "long": "input-schema-revision",
                "required": false,
                "short": null,
                "value_name": "REVISION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Opened Protocol Session identifier",
                "id": "session",
                "long": "session",
                "required": false,
                "short": null,
                "value_name": "SESSION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Explicit Target identifier",
                "id": "target",
                "long": "target",
                "required": false,
                "short": null,
                "value_name": "TARGET",
                "values": []
            },
            {
                "aliases": [],
                "help": "Value schema handle sha256 digest",
                "id": "value-schema-digest",
                "long": "value-schema-digest",
                "required": true,
                "short": null,
                "value_name": "DIGEST",
                "values": []
            },
            {
                "aliases": [],
                "help": "Value schema handle identifier",
                "id": "value-schema-id",
                "long": "value-schema-id",
                "required": true,
                "short": null,
                "value_name": "SCHEMA",
                "values": []
            },
            {
                "aliases": [],
                "help": "Value schema handle revision",
                "id": "value-schema-revision",
                "long": "value-schema-revision",
                "required": true,
                "short": null,
                "value_name": "REVISION",
                "values": []
            }
        ],
        "result_schema_id": "https://apppilotkit.dev/cli/v1/catalog.schema.json#/$defs/query",
        "result_fields": [
            "bytes",
            "value",
            "value_schema"
        ],
        "error_kinds": [
            "capabilityUnavailable",
            "cli.internalError",
            "cli.invalidInvocation",
            "cursorExpired",
            "incompatibleProtocol",
            "internalError",
            "invalidParams",
            "invalidRequest",
            "methodNotFound",
            "parseError",
            "resourceExhausted",
            "semantic.capabilityNotFound",
            "semantic.disclosureDenied",
            "semantic.schemaMismatch",
            "semantic.unavailable",
            "session.selectionRequired",
            "sessionExpired",
            "target.selectionRequired",
            "timeout",
            "transport.authenticationRequired"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "catalog",
            "schema"
        ],
        "aliases": [],
        "arguments": [
            {
                "aliases": [],
                "help": "Registered Semantic Capability identifier",
                "id": "capability",
                "long": "capability",
                "required": true,
                "short": null,
                "value_name": "CAPABILITY",
                "values": []
            },
            {
                "aliases": [],
                "help": "Declaration revision for the selected capability",
                "id": "declaration-revision",
                "long": "declaration-revision",
                "required": true,
                "short": null,
                "value_name": "REVISION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Live App schema handle sha256 digest",
                "id": "schema-digest",
                "long": "schema-digest",
                "required": true,
                "short": null,
                "value_name": "DIGEST",
                "values": []
            },
            {
                "aliases": [],
                "help": "Live App schema handle identifier",
                "id": "schema-id",
                "long": "schema-id",
                "required": true,
                "short": null,
                "value_name": "SCHEMA",
                "values": []
            },
            {
                "aliases": [],
                "help": "Live App schema handle revision",
                "id": "schema-revision",
                "long": "schema-revision",
                "required": true,
                "short": null,
                "value_name": "REVISION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Opened Protocol Session identifier",
                "id": "session",
                "long": "session",
                "required": false,
                "short": null,
                "value_name": "SESSION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Explicit Target identifier",
                "id": "target",
                "long": "target",
                "required": false,
                "short": null,
                "value_name": "TARGET",
                "values": []
            }
        ],
        "result_schema_id": "https://apppilotkit.dev/cli/v1/catalog.schema.json#/$defs/schema",
        "result_fields": [
            "document",
            "schema"
        ],
        "error_kinds": [
            "capabilityUnavailable",
            "cli.internalError",
            "cli.invalidInvocation",
            "cursorExpired",
            "incompatibleProtocol",
            "internalError",
            "invalidParams",
            "invalidRequest",
            "methodNotFound",
            "parseError",
            "resourceExhausted",
            "semantic.capabilityNotFound",
            "semantic.disclosureDenied",
            "semantic.schemaMismatch",
            "semantic.unavailable",
            "session.selectionRequired",
            "sessionExpired",
            "target.selectionRequired",
            "timeout",
            "transport.authenticationRequired"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "catalog",
            "show"
        ],
        "aliases": [],
        "arguments": [
            {
                "aliases": [],
                "help": "Registered Semantic Capability identifier",
                "id": "capability",
                "long": "capability",
                "required": true,
                "short": null,
                "value_name": "CAPABILITY",
                "values": []
            },
            {
                "aliases": [],
                "help": "Declaration revision for the selected capability",
                "id": "declaration-revision",
                "long": "declaration-revision",
                "required": true,
                "short": null,
                "value_name": "REVISION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Opened Protocol Session identifier",
                "id": "session",
                "long": "session",
                "required": false,
                "short": null,
                "value_name": "SESSION",
                "values": []
            },
            {
                "aliases": [],
                "help": "Explicit Target identifier",
                "id": "target",
                "long": "target",
                "required": false,
                "short": null,
                "value_name": "TARGET",
                "values": []
            }
        ],
        "result_schema_id": "https://apppilotkit.dev/cli/v1/catalog.schema.json#/$defs/show",
        "result_fields": [
            "declaration_revision",
            "id",
            "input_schema",
            "kind",
            "policy",
            "value_schema"
        ],
        "error_kinds": [
            "capabilityUnavailable",
            "cli.internalError",
            "cli.invalidInvocation",
            "cursorExpired",
            "incompatibleProtocol",
            "internalError",
            "invalidParams",
            "invalidRequest",
            "methodNotFound",
            "parseError",
            "resourceExhausted",
            "semantic.capabilityNotFound",
            "semantic.disclosureDenied",
            "semantic.schemaMismatch",
            "semantic.unavailable",
            "session.selectionRequired",
            "sessionExpired",
            "target.selectionRequired",
            "timeout",
            "transport.authenticationRequired"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "doctor"
        ],
        "aliases": [],
        "arguments": [],
        "result_schema_id": "https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/doctorReport",
        "result_fields": [
            "checks"
        ],
        "error_kinds": [
            "cli.internalError"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "schema"
        ],
        "aliases": [],
        "arguments": [],
        "result_schema_id": null,
        "result_fields": [],
        "error_kinds": [
            "cli.invalidInvocation"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "schema",
            "list"
        ],
        "aliases": [],
        "arguments": [],
        "result_schema_id": "https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/schemaList",
        "result_fields": [
            "schemas"
        ],
        "error_kinds": [
            "cli.internalError"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    },
    {
        "path": [
            "schema",
            "show"
        ],
        "aliases": [],
        "arguments": [
            {
                "aliases": [],
                "help": "Installed schema identifier",
                "id": "schema-id",
                "long": null,
                "required": true,
                "short": null,
                "value_name": "SCHEMA_ID",
                "values": []
            }
        ],
        "result_schema_id": "https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/schemaShow",
        "result_fields": [
            "schema",
            "schema_id"
        ],
        "error_kinds": [
            "cli.internalError",
            "cli.invalidInvocation"
        ],
        "side_effect": "read_only",
        "retry_safety": "safe"
    }
]
        )
        .as_array()
        .unwrap()
        .clone()
    );
    assert!(result["artifacts"].as_array().is_some_and(Vec::is_empty));
    assert!(result["next_actions"].as_array().is_some());
    assert!(
        result["data"]["schemas"]
            .as_array()
            .expect("schemas")
            .iter()
            .any(|schema| schema == "https://apppilotkit.dev/cli/v1/catalog.schema.json")
    );
}
