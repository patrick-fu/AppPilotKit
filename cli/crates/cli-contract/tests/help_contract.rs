use apppilotkit_cli_contract::{CliConfig, CliCore};

#[test]
fn help_is_a_complete_safe_starting_map_for_an_agent() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0")).expect("CLI initializes");

    let root = core.run(["fixture-cli", "--help"]);
    assert_eq!(root.exit_code, 0);
    assert!(root.stderr.is_empty());
    let root = String::from_utf8(root.stdout).expect("UTF-8 help");
    for required in [
        "Safe start: fixture-cli capabilities --output json",
        "Side effect: read_only",
        "Retry safety: safe",
        "Machine output:",
        "Bounded output:",
        "Recovery:",
        "fixture-cli doctor --output json --non-interactive",
        "catalog list --cursor",
    ] {
        assert!(
            root.contains(required),
            "missing root help text: {required}"
        );
    }
    assert!(!root.contains("does not invent cursor continuation commands"));

    let command = core.run(["fixture-cli", "schema", "show", "--help"]);
    assert_eq!(command.exit_code, 0);
    assert!(command.stderr.is_empty());
    let command = String::from_utf8(command.stdout).expect("UTF-8 help");
    for required in [
        "Result schema: https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/schemaShow",
        "Side effect: read_only",
        "Retry safety: safe",
        "Errors: cli.internalError, cli.invalidInvocation",
        "Recovery: fixture-cli schema show --help",
    ] {
        assert!(
            command.contains(required),
            "missing command help text: {required}"
        );
    }
}

#[test]
fn version_is_plain_stdout_with_a_zero_exit() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0")).expect("CLI initializes");
    let output = core.run(["fixture-cli", "--version"]);
    assert_eq!(output.exit_code, 0);
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "fixture-cli 0.1.0\n"
    );
}

#[test]
fn catalog_help_describes_the_generic_surface_without_a_target_catalog() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0")).expect("CLI initializes");
    let help = core.run(["fixture-cli", "catalog", "invoke", "--help"]);
    assert_eq!(help.exit_code, 0);
    let help = String::from_utf8(help.stdout).expect("UTF-8 help");
    for required in [
        "Result schema: https://apppilotkit.dev/cli/v1/catalog.schema.json#/$defs/invoke",
        "Side effect: app_mutation",
        "Retry safety: requires_idempotency_key",
        "--authorization-grant",
        "--input",
        "--session",
        "--target",
    ] {
        assert!(
            help.contains(required),
            "missing catalog help text: {required}"
        );
    }
    assert!(!help.contains("account.delete"));
    assert!(!help.contains("config.current"));
}
