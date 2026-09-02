use apppilotkit_cli_contract::{CliConfig, CliCore};
use apppilotkit_production_composition::BrokerCatalogRuntime;
use std::{
    io::{self, Write},
    sync::Arc,
};

fn main() {
    let runtime = Arc::new(BrokerCatalogRuntime::current_user());
    let core = match CliCore::with_catalog_runtime(
        CliConfig::new("apppilotkit", env!("CARGO_PKG_VERSION")),
        runtime,
    ) {
        Ok(core) => core,
        Err(_) => std::process::exit(70),
    };
    let output = core.run(std::env::args_os());
    let _ = io::stdout().write_all(&output.stdout);
    let _ = io::stderr().write_all(&output.stderr);
    std::process::exit(i32::from(output.exit_code));
}
