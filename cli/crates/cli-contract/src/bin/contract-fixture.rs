use apppilotkit_cli_contract::{CliConfig, CliCore};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

#[cfg(unix)]
mod fixture_runtime;

fn main() -> ExitCode {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let executable = arguments
        .first()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "apppilotkit-cli-contract-fixture".to_owned());
    let config = CliConfig::new(executable, env!("CARGO_PKG_VERSION"));
    #[cfg(unix)]
    let core = if let Some(socket) = std::env::var_os("APPPILOTKIT_CONTRACT_FIXTURE_SOCKET") {
        CliCore::with_catalog_runtime(
            config,
            Arc::new(fixture_runtime::FixtureCatalogRuntime::new(PathBuf::from(
                socket,
            ))),
        )
    } else {
        CliCore::new(config)
    };
    #[cfg(not(unix))]
    let core = CliCore::new(config);
    let core = match core {
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
