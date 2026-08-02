use apppilotkit_cli_contract::{CliConfig, CliCore};
use serde_json::Value;

#[test]
fn every_next_action_is_exact_bounded_and_invokes_an_installed_safe_command() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0")).expect("CLI initializes");
    let output = core.run(["fixture-cli", "capabilities", "--output", "json"]);
    let result: Value = serde_json::from_slice(&output.stdout).expect("capabilities result");
    let next_actions = result["next_actions"].as_array().expect("Next Actions");

    assert_eq!(next_actions.len(), 2);
    for next_action in next_actions {
        let argv = next_action["argv"]
            .as_array()
            .expect("exact argv array")
            .iter()
            .map(|value| value.as_str().expect("argv token"))
            .collect::<Vec<_>>();
        assert_eq!(argv.first(), Some(&"fixture-cli"));
        assert_eq!(next_action["side_effect"], "read_only");
        assert_eq!(next_action["retry_safety"], "safe");
        assert!(next_action["preconditions"].is_array());
        assert!(
            next_action["reason"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );

        let invoked = core.run(argv);
        assert_eq!(
            invoked.exit_code, 0,
            "Next Action invokes an installed command"
        );
        assert!(invoked.stderr.is_empty());
        let invoked: Value =
            serde_json::from_slice(&invoked.stdout).expect("Next Action machine result");
        assert_eq!(invoked["status"], "succeeded");
    }
}
