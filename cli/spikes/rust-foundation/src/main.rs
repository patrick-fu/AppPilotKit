use apppilotkit_rust_foundation_spike::run_cli;
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let execution = run_cli(std::env::args_os());
    if let Err(error) = std::io::stdout().write_all(&execution.stdout) {
        let _ = writeln!(std::io::stderr(), "failed to write stdout: {error}");
        return ExitCode::from(1);
    }
    if let Err(error) = std::io::stderr().write_all(&execution.stderr) {
        let _ = writeln!(std::io::stderr(), "failed to write stderr: {error}");
        return ExitCode::from(1);
    }
    ExitCode::from(execution.exit_code)
}
