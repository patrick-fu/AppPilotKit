use apppilotkit_cli_contract::{CliConfig, CliCore};
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let executable = arguments
        .first()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "apppilotkit-cli-contract-fixture".to_owned());
    let core = match CliCore::new(CliConfig::new(executable, env!("CARGO_PKG_VERSION"))) {
        Ok(core) => core,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "{error}");
            return ExitCode::FAILURE;
        }
    };
    let output = core.run(arguments);
    if std::io::stdout().lock().write_all(&output.stdout).is_err()
        || std::io::stderr().lock().write_all(&output.stderr).is_err()
    {
        return ExitCode::FAILURE;
    }
    ExitCode::from(output.exit_code)
}
