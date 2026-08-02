use crate::{SpikeOutcome, SpikeResult};
use clap::{Arg, Command, builder::PossibleValuesParser};
use serde::Serialize;
use serde_json::{Value, json};
use std::ffi::OsString;

#[derive(Debug, PartialEq, Eq)]
pub struct CliExecution {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
}

#[must_use]
pub fn command_model() -> Command {
    Command::new("spike")
        .about("Internal AppPilotKit Rust foundation spike")
        .version(env!("CARGO_PKG_VERSION"))
        .disable_help_subcommand(true)
        .subcommand_required(true)
        .subcommand(
            Command::new("emit")
                .about("Emit spike-only structured output")
                .arg(
                    Arg::new("format")
                        .long("format")
                        .short('f')
                        .help("Select one JSON document or a JSONL event stream")
                        .value_parser(PossibleValuesParser::new(["document", "jsonl"]))
                        .default_value("document"),
                )
                .arg(
                    Arg::new("summary")
                        .long("summary")
                        .help("Set the UTF-8 result summary")
                        .default_value("spike completed"),
                ),
        )
        .subcommand(Command::new("manifest").about("Print the offline spike command manifest"))
}

#[must_use]
pub fn command_manifest() -> Value {
    let mut model = command_model();
    model.build();
    let mut commands = Vec::new();
    collect_commands(&model, String::new(), &mut commands);
    commands.sort_by(|left, right| left.path.cmp(&right.path));
    json!({
        "schema_version": 1,
        "commands": commands,
    })
}

#[must_use]
pub fn run_cli<I, T>(arguments: I) -> CliExecution
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = match command_model().try_get_matches_from(arguments) {
        Ok(matches) => matches,
        Err(error) => {
            return CliExecution {
                stdout: Vec::new(),
                stderr: newline_terminated(error.to_string().into_bytes()),
                exit_code: 2,
            };
        }
    };

    match matches.subcommand() {
        Some(("manifest", _)) => successful_json(&command_manifest()),
        Some(("emit", matches)) => {
            let summary = matches
                .get_one::<String>("summary")
                .expect("clap supplies the summary default")
                .clone();
            let result = SpikeResult {
                schema_version: 1,
                outcome: SpikeOutcome::Succeeded,
                summary,
            };
            match matches
                .get_one::<String>("format")
                .expect("clap supplies the format default")
                .as_str()
            {
                "document" => successful_json(&result),
                "jsonl" => successful_jsonl(&result),
                _ => unreachable!("clap restricts format values"),
            }
        }
        _ => unreachable!("clap requires a known subcommand"),
    }
}

fn successful_json(value: &impl Serialize) -> CliExecution {
    let stdout = serde_json::to_vec(value).expect("spike-owned values serialize");
    CliExecution {
        stdout: newline_terminated(stdout),
        stderr: Vec::new(),
        exit_code: 0,
    }
}

fn successful_jsonl(result: &SpikeResult) -> CliExecution {
    let events = [
        json!({
            "sequence": 0,
            "terminal": false,
            "type": "run.started",
        }),
        json!({
            "sequence": 1,
            "terminal": true,
            "type": "run.succeeded",
            "result": result,
        }),
    ];
    let mut stdout = Vec::new();
    for event in events {
        stdout.extend(serde_json::to_vec(&event).expect("spike-owned events serialize"));
        stdout.push(b'\n');
    }
    CliExecution {
        stdout,
        stderr: Vec::new(),
        exit_code: 0,
    }
}

fn newline_terminated(mut bytes: Vec<u8>) -> Vec<u8> {
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes
}

#[derive(Debug, Serialize)]
struct ManifestCommand {
    path: String,
    about: Option<String>,
    arguments: Vec<ManifestArgument>,
}

#[derive(Debug, Serialize)]
struct ManifestArgument {
    id: String,
    long: Option<String>,
    short: Option<char>,
    aliases: Vec<String>,
    required: bool,
    help: Option<String>,
}

fn collect_commands(command: &Command, parent: String, manifest: &mut Vec<ManifestCommand>) {
    if command.is_hide_set() {
        return;
    }
    let path = if parent.is_empty() {
        command.get_name().to_owned()
    } else {
        format!("{parent} {}", command.get_name())
    };
    let mut arguments = command
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
        .map(|argument| ManifestArgument {
            id: argument.get_id().to_string(),
            long: argument.get_long().map(str::to_owned),
            short: argument.get_short(),
            aliases: argument
                .get_visible_aliases()
                .unwrap_or_default()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            required: argument.is_required_set(),
            help: argument.get_help().map(ToString::to_string),
        })
        .collect::<Vec<_>>();
    arguments.sort_by(|left, right| left.id.cmp(&right.id));
    manifest.push(ManifestCommand {
        path: path.clone(),
        about: command.get_about().map(ToString::to_string),
        arguments,
    });
    for subcommand in command.get_subcommands() {
        collect_commands(subcommand, path.clone(), manifest);
    }
}
