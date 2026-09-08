use apppilotkit_production_composition::{
    BrokerControlClient, PrepareError, prepare_target, release_target, render_prepare_error,
};
use std::io::{self, Read, Write};

fn main() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments.len() != 3 || arguments[2] != "--output=json" {
        emit(render_prepare_error(&PrepareError::InvalidInvocation));
        std::process::exit(2);
    }
    let is_prepare = arguments[1] == "--request-fd=0";
    let is_release = arguments[1] == "--release-fd=0";
    if !is_prepare && !is_release {
        emit(render_prepare_error(&PrepareError::InvalidInvocation));
        std::process::exit(2);
    }
    let mut input = Vec::new();
    let result = io::stdin()
        .take(65_537)
        .read_to_end(&mut input)
        .map_err(|_| PrepareError::Io)
        .and_then(|bytes_read| {
            if bytes_read > 65_536 {
                Err(PrepareError::InvalidInvocation)
            } else {
                Ok(())
            }
        });
    if let Err(error) = result {
        emit(render_prepare_error(&error));
        std::process::exit(1);
    }
    let client = BrokerControlClient::current_user();
    if is_prepare {
        match prepare_target(&input, &client) {
            Ok(ready) => emit(
                serde_json::json!({"schema_version":"1.0","status":"succeeded","ready_target":{"schema_version":"1.0","target":ready.target,"issued_at_unix_ms":ready.issued_at_unix_ms,"expires_at_unix_ms":ready.expires_at_unix_ms}}),
            ),
            Err(error) => {
                emit(render_prepare_error(&error));
                std::process::exit(1);
            }
        }
    } else {
        match release_target(&input, &client) {
            Ok(()) => emit(serde_json::json!({"schema_version":"1.0","status":"succeeded"})),
            Err(error) => {
                emit(render_prepare_error(&error));
                std::process::exit(1);
            }
        }
    }
}

fn emit(value: serde_json::Value) {
    let mut output = io::stdout();
    let _ = serde_json::to_writer(&mut output, &value);
    let _ = output.write_all(b"\n");
}
