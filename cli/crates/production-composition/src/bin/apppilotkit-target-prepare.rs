use apppilotkit_production_composition::{
    BrokerControlClient, PrepareError, prepare_target, render_prepare_error,
};
use std::io::{self, Read, Write};

fn main() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments.len() != 3 || arguments[1] != "--request-fd=0" || arguments[2] != "--output=json" {
        emit(render_prepare_error(&PrepareError::InvalidInvocation));
        std::process::exit(2);
    }
    let mut input = Vec::new();
    let result = io::stdin()
        .take(65_537)
        .read_to_end(&mut input)
        .map_err(|_| PrepareError::Io)
        .and_then(|_| {
            if input.len() > 65_536 {
                Err(PrepareError::InvalidInvocation)
            } else {
                prepare_target(&input, &BrokerControlClient::current_user())
            }
        });
    match result {
        Ok(ready) => emit(
            serde_json::json!({"schema_version":"1.0","status":"succeeded","ready_target":{"schema_version":"1.0","target":ready.target,"issued_at_unix_ms":ready.issued_at_unix_ms,"expires_at_unix_ms":ready.expires_at_unix_ms}}),
        ),
        Err(error) => {
            emit(render_prepare_error(&error));
            std::process::exit(1);
        }
    }
}

fn emit(value: serde_json::Value) {
    let mut output = io::stdout();
    let _ = serde_json::to_writer(&mut output, &value);
    let _ = output.write_all(b"\n");
}
