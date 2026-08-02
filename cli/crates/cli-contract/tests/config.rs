use apppilotkit_cli_contract::{CliConfig, CliCore, InitError};

#[test]
fn initialization_rejects_values_that_would_make_runtime_results_invalid() {
    assert!(matches!(
        CliCore::new(CliConfig::new("fixture-cli", "1")),
        Err(InitError::InvalidVersion)
    ));
    assert!(matches!(
        CliCore::new(CliConfig::new("x".repeat(4097), "0.1.0")),
        Err(InitError::InvalidExecutable)
    ));
    for invalid_semver in ["1.0.0-01", "1.0.0-alpha..1", "1.0.0+build..1"] {
        assert!(matches!(
            CliCore::new(CliConfig::new("fixture-cli", invalid_semver)),
            Err(InitError::InvalidVersion)
        ));
    }

    CliCore::new(CliConfig::new("fixture-cli", "1.2.3-rc.1+build.7"))
        .expect("SemVer prerelease and build metadata are accepted");
}
