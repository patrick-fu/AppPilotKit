use crate::result::{RetrySafety, SideEffect};
use clap::{Arg, ArgAction, Command, builder::PossibleValuesParser};
use serde::Serialize;
use std::ffi::OsString;

pub(crate) const OUTPUT_MODES: &[&str] = &["human", "json", "jsonl"];

const GLOBAL_ARGUMENTS: &[ArgumentSpec] = &[
    ArgumentSpec {
        id: "help",
        long: Some("help"),
        short: Some('h'),
        aliases: &[],
        value_name: None,
        help: "Print help for the current command",
        values: &[],
        required: false,
        global: true,
        action: ArgumentAction::Help,
    },
    ArgumentSpec {
        id: "non-interactive",
        long: Some("non-interactive"),
        short: None,
        aliases: &[],
        value_name: None,
        help: "Never prompt or consume implicit input",
        values: &[],
        required: false,
        global: true,
        action: ArgumentAction::SetTrue,
    },
    ArgumentSpec {
        id: "output",
        long: Some("output"),
        short: None,
        aliases: &[],
        value_name: Some("MODE"),
        help: "Select deterministic human, JSON, or JSONL output",
        values: OUTPUT_MODES,
        required: false,
        global: true,
        action: ArgumentAction::Set,
    },
    ArgumentSpec {
        id: "version",
        long: Some("version"),
        short: Some('V'),
        aliases: &[],
        value_name: None,
        help: "Print the CLI version",
        values: &[],
        required: false,
        global: false,
        action: ArgumentAction::Version,
    },
];

const SCHEMA_SHOW_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    id: "schema-id",
    long: None,
    short: None,
    aliases: &[],
    value_name: Some("SCHEMA_ID"),
    help: "Installed schema identifier",
    values: &[],
    required: true,
    global: false,
    action: ArgumentAction::Set,
}];

const SCHEMA_CHILDREN: &[CommandSpec] = &[
    CommandSpec {
        name: "list",
        aliases: &[],
        about: "List every installed CLI contract schema",
        arguments: &[],
        children: &[],
        result_schema_id: Some(
            "https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/schemaList",
        ),
        result_fields: &["schemas"],
        error_kinds: &["cli.internalError"],
        side_effect: SideEffect::ReadOnly,
        retry_safety: RetrySafety::Safe,
    },
    CommandSpec {
        name: "show",
        aliases: &[],
        about: "Show one exact embedded CLI contract schema",
        arguments: SCHEMA_SHOW_ARGUMENTS,
        children: &[],
        result_schema_id: Some(
            "https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/schemaShow",
        ),
        result_fields: &["schema", "schema_id"],
        error_kinds: &["cli.internalError", "cli.invalidInvocation"],
        side_effect: SideEffect::ReadOnly,
        retry_safety: RetrySafety::Safe,
    },
];

const ROOT_CHILDREN: &[CommandSpec] = &[
    CommandSpec {
        name: "capabilities",
        aliases: &[],
        about: "Describe the complete installed CLI contract",
        arguments: &[],
        children: &[],
        result_schema_id: Some("https://apppilotkit.dev/cli/v1/capability-manifest.schema.json"),
        result_fields: &[
            "cli_version",
            "commands",
            "error_kinds",
            "executable",
            "global_arguments",
            "output_modes",
            "retry_safety_values",
            "schema_version",
            "schemas",
            "side_effect_classes",
        ],
        error_kinds: &["cli.internalError"],
        side_effect: SideEffect::ReadOnly,
        retry_safety: RetrySafety::Safe,
    },
    CommandSpec {
        name: "doctor",
        aliases: &[],
        about: "Check local CLI prerequisites without contacting a device",
        arguments: &[],
        children: &[],
        result_schema_id: Some(
            "https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/doctorReport",
        ),
        result_fields: &["checks"],
        error_kinds: &["cli.internalError"],
        side_effect: SideEffect::ReadOnly,
        retry_safety: RetrySafety::Safe,
    },
    CommandSpec {
        name: "schema",
        aliases: &[],
        about: "Discover the installed machine contract schemas",
        arguments: &[],
        children: SCHEMA_CHILDREN,
        result_schema_id: None,
        result_fields: &[],
        error_kinds: &["cli.invalidInvocation"],
        side_effect: SideEffect::ReadOnly,
        retry_safety: RetrySafety::Safe,
    },
];

pub(crate) const SCHEMA_IDS: &[&str] = &[
    "https://apppilotkit.dev/cli/v1/artifact.schema.json",
    "https://apppilotkit.dev/cli/v1/capability-manifest.schema.json",
    "https://apppilotkit.dev/cli/v1/disclosure.schema.json",
    "https://apppilotkit.dev/cli/v1/discovery.schema.json",
    "https://apppilotkit.dev/cli/v1/error.schema.json",
    "https://apppilotkit.dev/cli/v1/jsonl-event.schema.json",
    "https://apppilotkit.dev/cli/v1/machine-result.schema.json",
    "https://apppilotkit.dev/cli/v1/next-action.schema.json",
];

#[derive(Clone, Copy)]
enum ArgumentAction {
    Help,
    Set,
    SetTrue,
    Version,
}

#[derive(Clone, Copy)]
struct ArgumentSpec {
    id: &'static str,
    long: Option<&'static str>,
    short: Option<char>,
    aliases: &'static [&'static str],
    value_name: Option<&'static str>,
    help: &'static str,
    values: &'static [&'static str],
    required: bool,
    global: bool,
    action: ArgumentAction,
}

#[derive(Clone, Copy)]
struct CommandSpec {
    name: &'static str,
    aliases: &'static [&'static str],
    about: &'static str,
    arguments: &'static [ArgumentSpec],
    children: &'static [CommandSpec],
    result_schema_id: Option<&'static str>,
    result_fields: &'static [&'static str],
    error_kinds: &'static [&'static str],
    side_effect: SideEffect,
    retry_safety: RetrySafety,
}

#[derive(Debug, Serialize)]
pub(crate) struct CapabilityManifest<'a> {
    pub schema_version: &'static str,
    pub cli_version: &'a str,
    pub executable: &'a str,
    pub output_modes: &'static [&'static str],
    pub global_arguments: Vec<ManifestArgument>,
    pub commands: Vec<ManifestCommand>,
    pub schemas: &'static [&'static str],
    pub side_effect_classes: Vec<&'static str>,
    pub retry_safety_values: Vec<&'static str>,
    pub error_kinds: Vec<ManifestErrorKind>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ManifestArgument {
    id: &'static str,
    long: Option<&'static str>,
    short: Option<char>,
    aliases: Vec<&'static str>,
    value_name: Option<&'static str>,
    values: &'static [&'static str],
    required: bool,
    help: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct ManifestCommand {
    path: Vec<&'static str>,
    aliases: &'static [&'static str],
    about: &'static str,
    arguments: Vec<ManifestArgument>,
    result_schema_id: Option<&'static str>,
    result_fields: &'static [&'static str],
    error_kinds: &'static [&'static str],
    side_effect: SideEffect,
    retry_safety: RetrySafety,
}

#[derive(Debug, Serialize)]
pub(crate) struct ManifestErrorKind {
    pub(crate) kind: &'static str,
    pub(crate) exit_code: u8,
}

pub(crate) fn command_model(executable: &str, cli_version: &str) -> Command {
    let mut root = Command::new(executable.to_owned())
        .bin_name(executable.to_owned())
        .about("Inspect and operate opted-in mobile apps through a self-guiding CLI")
        .after_long_help(root_guidance(executable))
        .version(cli_version.to_owned())
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .disable_version_flag(true)
        .subcommand_required(false);
    for argument in GLOBAL_ARGUMENTS {
        root = root.arg(build_argument(*argument));
    }
    for child in ROOT_CHILDREN {
        root = root.subcommand(build_command(*child, executable, &[]));
    }
    root
}

pub(crate) fn capability_manifest<'a>(
    executable: &'a str,
    cli_version: &'a str,
) -> CapabilityManifest<'a> {
    let mut commands = vec![ManifestCommand {
        path: Vec::new(),
        aliases: &[],
        about: "Inspect and operate opted-in mobile apps through a self-guiding CLI",
        arguments: GLOBAL_ARGUMENTS
            .iter()
            .copied()
            .filter(|argument| !argument.global)
            .map(manifest_argument)
            .collect(),
        result_schema_id: None,
        result_fields: &[],
        error_kinds: &["cli.invalidInvocation"],
        side_effect: SideEffect::ReadOnly,
        retry_safety: RetrySafety::Safe,
    }];
    collect_manifest_commands(ROOT_CHILDREN, &[], &mut commands);
    commands.sort_by(|left, right| left.path.cmp(&right.path));
    CapabilityManifest {
        schema_version: "1.0",
        cli_version,
        executable,
        output_modes: OUTPUT_MODES,
        global_arguments: GLOBAL_ARGUMENTS
            .iter()
            .copied()
            .filter(|argument| argument.global)
            .map(manifest_argument)
            .collect(),
        commands,
        schemas: SCHEMA_IDS,
        side_effect_classes: SideEffect::ALL
            .iter()
            .copied()
            .map(SideEffect::as_str)
            .collect(),
        retry_safety_values: RetrySafety::ALL
            .iter()
            .copied()
            .map(RetrySafety::as_str)
            .collect(),
        error_kinds: vec![
            ManifestErrorKind {
                kind: "action.outcomeUnknown",
                exit_code: 5,
            },
            ManifestErrorKind {
                kind: "artifact.alreadyExists",
                exit_code: 7,
            },
            ManifestErrorKind {
                kind: "capabilityUnavailable",
                exit_code: 6,
            },
            ManifestErrorKind {
                kind: "cli.internalError",
                exit_code: 1,
            },
            ManifestErrorKind {
                kind: "cli.invalidInvocation",
                exit_code: 2,
            },
            ManifestErrorKind {
                kind: "cursorExpired",
                exit_code: 4,
            },
            ManifestErrorKind {
                kind: "incompatibleProtocol",
                exit_code: 6,
            },
            ManifestErrorKind {
                kind: "internalError",
                exit_code: 1,
            },
            ManifestErrorKind {
                kind: "invalidParams",
                exit_code: 6,
            },
            ManifestErrorKind {
                kind: "invalidRequest",
                exit_code: 6,
            },
            ManifestErrorKind {
                kind: "methodNotFound",
                exit_code: 6,
            },
            ManifestErrorKind {
                kind: "parseError",
                exit_code: 6,
            },
            ManifestErrorKind {
                kind: "resourceExhausted",
                exit_code: 1,
            },
            ManifestErrorKind {
                kind: "run.cancelled",
                exit_code: 130,
            },
            ManifestErrorKind {
                kind: "sessionExpired",
                exit_code: 4,
            },
            ManifestErrorKind {
                kind: "target.selectionRequired",
                exit_code: 4,
            },
            ManifestErrorKind {
                kind: "timeout",
                exit_code: 1,
            },
            ManifestErrorKind {
                kind: "transport.authenticationRequired",
                exit_code: 3,
            },
            ManifestErrorKind {
                kind: "ui.referenceNotFound",
                exit_code: 4,
            },
            ManifestErrorKind {
                kind: "ui.snapshotExpired",
                exit_code: 4,
            },
        ],
    }
}

pub(crate) fn recognized_command_prefix(arguments: &[OsString]) -> Vec<String> {
    let mut positional = Vec::new();
    let mut index = 1;
    while index < arguments.len() {
        let Some(argument) = arguments[index].to_str() else {
            break;
        };
        if argument == "--" {
            break;
        }
        if argument == "--output" {
            index += 2;
            continue;
        }
        if argument.starts_with("--output=") || argument.starts_with('-') {
            index += 1;
            continue;
        }
        positional.push(argument);
        index += 1;
    }

    let mut prefix = Vec::new();
    let mut candidates = ROOT_CHILDREN;
    for token in positional {
        let Some(command) = candidates.iter().find(|command| command.name == token) else {
            break;
        };
        prefix.push(command.name.to_owned());
        candidates = command.children;
        if candidates.is_empty() {
            break;
        }
    }
    prefix
}

pub(crate) fn command_annotations(path: &[String]) -> Option<(SideEffect, RetrySafety)> {
    if path.is_empty() {
        return Some((SideEffect::ReadOnly, RetrySafety::Safe));
    }
    command_spec(path).map(|spec| (spec.side_effect, spec.retry_safety))
}

pub(crate) fn command_result_schema_id(path: &[String]) -> Option<&'static str> {
    command_spec(path).and_then(|spec| spec.result_schema_id)
}

fn command_spec(path: &[String]) -> Option<CommandSpec> {
    let mut candidates = ROOT_CHILDREN;
    let mut matched = None;
    for token in path {
        let spec = candidates.iter().find(|spec| spec.name == token)?;
        matched = Some(*spec);
        candidates = spec.children;
    }
    matched
}

fn build_command(spec: CommandSpec, executable: &str, parent: &[&str]) -> Command {
    let mut path = parent.to_vec();
    path.push(spec.name);
    let mut command = Command::new(spec.name)
        .visible_aliases(spec.aliases)
        .about(spec.about)
        .after_long_help(command_guidance(spec, executable, &path))
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .disable_version_flag(true);
    for argument in spec.arguments {
        command = command.arg(build_argument(*argument));
    }
    for child in spec.children {
        command = command.subcommand(build_command(*child, executable, &path));
    }
    command
}

fn root_guidance(executable: &str) -> String {
    format!(
        "Safe start: {executable} capabilities --output json\n\
         Side effect: read_only\n\
         Retry safety: safe\n\
         Machine output: use --output json for one Machine Result or --output jsonl for started and terminal events; structured results use stdout and diagnostics use stderr.\n\
         Bounded output: inspect disclosure.truncated and next_actions; this discovery slice does not invent cursor continuation commands.\n\
         Recovery: {executable} schema list --output json; {executable} doctor --output json --non-interactive"
    )
}

fn command_guidance(spec: CommandSpec, executable: &str, path: &[&str]) -> String {
    let result_schema = spec.result_schema_id.unwrap_or("none (command group)");
    let errors = if spec.error_kinds.is_empty() {
        "none".to_owned()
    } else {
        spec.error_kinds.join(", ")
    };
    format!(
        "Result schema: {result_schema}\n\
         Side effect: {}\n\
         Retry safety: {}\n\
         Errors: {errors}\n\
         Machine output: use --output json or --output jsonl; structured results use stdout and diagnostics use stderr.\n\
         Bounded output: inspect disclosure.truncated and next_actions.\n\
         Recovery: {executable} {} --help\n\
         Safe discovery: {executable} capabilities --output json",
        spec.side_effect.as_str(),
        spec.retry_safety.as_str(),
        path.join(" "),
    )
}

fn build_argument(spec: ArgumentSpec) -> Arg {
    let mut argument = Arg::new(spec.id)
        .help(spec.help)
        .required(spec.required)
        .global(spec.global)
        .action(match spec.action {
            ArgumentAction::Help => ArgAction::Help,
            ArgumentAction::Set => ArgAction::Set,
            ArgumentAction::SetTrue => ArgAction::SetTrue,
            ArgumentAction::Version => ArgAction::Version,
        });
    if let Some(long) = spec.long {
        argument = argument.long(long);
    }
    if let Some(short) = spec.short {
        argument = argument.short(short);
    }
    if !spec.aliases.is_empty() {
        argument = argument.visible_aliases(spec.aliases);
    }
    if let Some(value_name) = spec.value_name {
        argument = argument.value_name(value_name);
    }
    if !spec.values.is_empty() {
        argument = argument.value_parser(PossibleValuesParser::new(spec.values.iter().copied()));
    }
    if spec.id == "output" {
        argument = argument.default_value("human");
    }
    argument
}

fn collect_manifest_commands(
    specs: &'static [CommandSpec],
    parent: &[&'static str],
    output: &mut Vec<ManifestCommand>,
) {
    for spec in specs {
        let mut path = parent.to_vec();
        path.push(spec.name);
        output.push(ManifestCommand {
            path: path.clone(),
            aliases: spec.aliases,
            about: spec.about,
            arguments: spec
                .arguments
                .iter()
                .copied()
                .map(manifest_argument)
                .collect(),
            result_schema_id: spec.result_schema_id,
            result_fields: spec.result_fields,
            error_kinds: spec.error_kinds,
            side_effect: spec.side_effect,
            retry_safety: spec.retry_safety,
        });
        collect_manifest_commands(spec.children, &path, output);
    }
}

fn manifest_argument(spec: ArgumentSpec) -> ManifestArgument {
    ManifestArgument {
        id: spec.id,
        long: spec.long,
        short: spec.short,
        aliases: spec.aliases.to_vec(),
        value_name: spec.value_name,
        values: spec.values,
        required: spec.required,
        help: spec.help,
    }
}

#[cfg(test)]
mod tests {
    use super::{ManifestArgument, capability_manifest, command_model, command_result_schema_id};
    use clap::{Arg, Command};

    #[test]
    fn manifest_is_an_exact_projection_of_the_built_parser() {
        let mut parser = command_model("fixture-cli", "0.1.0");
        parser.build();
        let manifest = capability_manifest("fixture-cli", "0.1.0");

        assert!(
            manifest
                .commands
                .windows(2)
                .all(|pair| pair[0].path < pair[1].path)
        );
        assert!(
            manifest
                .error_kinds
                .windows(2)
                .all(|pair| pair[0].kind < pair[1].kind)
        );
        assert!(
            manifest
                .global_arguments
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id)
        );
        for command in &manifest.commands {
            assert!(
                command
                    .arguments
                    .windows(2)
                    .all(|pair| pair[0].id < pair[1].id)
            );
            assert!(command.error_kinds.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(
                command
                    .result_fields
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
            );
        }

        let parser_paths = command_paths(&parser, &[]);
        let manifest_paths = manifest
            .commands
            .iter()
            .map(|command| command.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(parser_paths, manifest_paths);

        let parser_globals = parser
            .get_arguments()
            .filter(|argument| argument.is_global_set())
            .collect::<Vec<_>>();
        assert_arguments(&parser_globals, &manifest.global_arguments);

        for manifest_command in &manifest.commands {
            let parser_command = command_at(&parser, &manifest_command.path);
            assert_eq!(
                parser_command.get_all_aliases().collect::<Vec<_>>(),
                manifest_command.aliases
            );
            let local_arguments = parser_command
                .get_arguments()
                .filter(|argument| !argument.is_global_set())
                .collect::<Vec<_>>();
            assert_arguments(&local_arguments, &manifest_command.arguments);
        }
    }

    #[test]
    fn command_result_schema_lookup_uses_the_registry_declaration() {
        assert_eq!(
            command_result_schema_id(&["schema".to_owned(), "show".to_owned()]),
            Some("https://apppilotkit.dev/cli/v1/discovery.schema.json#/$defs/schemaShow")
        );
        assert_eq!(command_result_schema_id(&["schema".to_owned()]), None);
    }

    fn command_paths<'a>(command: &'a Command, parent: &[&'a str]) -> Vec<Vec<&'a str>> {
        let mut paths = vec![parent.to_vec()];
        for child in command.get_subcommands() {
            let mut path = parent.to_vec();
            path.push(child.get_name());
            paths.extend(command_paths(child, &path));
        }
        paths.sort();
        paths
    }

    fn command_at<'a>(root: &'a Command, path: &[&str]) -> &'a Command {
        path.iter().fold(root, |command, name| {
            command
                .get_subcommands()
                .find(|child| child.get_name() == *name)
                .expect("manifest command exists in parser")
        })
    }

    fn assert_arguments(parser: &[&Arg], manifest: &[ManifestArgument]) {
        assert_eq!(parser.len(), manifest.len());
        for (parser, manifest) in parser.iter().zip(manifest) {
            assert_eq!(parser.get_id().as_str(), manifest.id);
            assert_eq!(parser.get_long(), manifest.long);
            assert_eq!(parser.get_short(), manifest.short);
            assert_eq!(
                parser.get_all_aliases().unwrap_or_default(),
                manifest.aliases
            );
            assert_eq!(parser.is_required_set(), manifest.required);
        }
    }
}
