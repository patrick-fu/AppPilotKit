fn main() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments.len() != 2 || arguments[1] != "--serve" {
        std::process::exit(2);
    }
    if apppilotkit_production_composition::run_broker().is_err() {
        std::process::exit(70);
    }
}
