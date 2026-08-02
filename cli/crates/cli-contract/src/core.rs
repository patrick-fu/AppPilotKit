use crate::contracts::ContractCatalog;
use crate::registry::{
    capability_manifest, command_annotations, command_model, command_result_schema_id,
    recognized_command_prefix,
};
use crate::result::{
    Disclosure, DoctorCheck, DoctorReport, HandlerOutcome, InvocationMetadata, MachineResult,
    NextAction, OutcomeContext, RetrySafety, SideEffect, StructuredError,
};
use serde_json::to_value;
use std::ffi::OsString;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliConfig {
    executable: String,
    cli_version: String,
}

impl CliConfig {
    #[must_use]
    pub fn new(executable: impl Into<String>, cli_version: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
            cli_version: cli_version.into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitError {
    EmptyExecutable,
    InvalidExecutable,
    EmptyVersion,
    InvalidVersion,
    Schema(String),
}

impl std::fmt::Display for InitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyExecutable => formatter.write_str("CLI executable token must not be empty"),
            Self::InvalidExecutable => {
                formatter.write_str("CLI executable token does not satisfy the machine contract")
            }
            Self::EmptyVersion => formatter.write_str("CLI version must not be empty"),
            Self::InvalidVersion => {
                formatter.write_str("CLI version must be a valid semantic version")
            }
            Self::Schema(error) => write!(formatter, "invalid embedded CLI contract: {error}"),
        }
    }
}

impl std::error::Error for InitError {}

#[derive(Debug, PartialEq, Eq)]
pub struct ProcessOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
}

#[derive(Debug)]
pub struct CliCore {
    config: CliConfig,
    contracts: ContractCatalog,
    next_run_id: AtomicU64,
}

impl CliCore {
    pub fn new(config: CliConfig) -> Result<Self, InitError> {
        if config.executable.is_empty() {
            return Err(InitError::EmptyExecutable);
        }
        if config.cli_version.is_empty() {
            return Err(InitError::EmptyVersion);
        }
        let contracts = ContractCatalog::new().map_err(InitError::Schema)?;
        if contracts
            .validate(
                "https://apppilotkit.dev/cli/v1/capability-manifest.schema.json#/$defs/executable",
                &serde_json::json!(&config.executable),
            )
            .is_err()
        {
            return Err(InitError::InvalidExecutable);
        }
        if contracts
            .validate(
                "https://apppilotkit.dev/cli/v1/machine-result.schema.json#/$defs/semver",
                &serde_json::json!(&config.cli_version),
            )
            .is_err()
        {
            return Err(InitError::InvalidVersion);
        }
        Ok(Self {
            config,
            contracts,
            next_run_id: AtomicU64::new(1),
        })
    }

    #[must_use]
    pub fn run<I, T>(&self, arguments: I) -> ProcessOutput
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let arguments = arguments
            .into_iter()
            .map(Into::into)
            .collect::<Vec<OsString>>();
        let machine_output = unambiguous_machine_output(&arguments);
        let matches = match command_model(&self.config.executable, &self.config.cli_version)
            .try_get_matches_from(arguments.clone())
        {
            Ok(matches) => matches,
            Err(error) if !error.use_stderr() => return clap_output(error),
            Err(error) => {
                if let Some(output_mode) = machine_output {
                    return self.invalid_invocation_output(
                        recognized_command_prefix(&arguments),
                        output_mode,
                        false,
                    );
                }
                return clap_output(error);
            }
        };
        let output_mode = OutputMode::from_matches(&matches);

        match matches.subcommand() {
            Some(("capabilities", _)) => {
                let manifest =
                    capability_manifest(&self.config.executable, &self.config.cli_version);
                let returned_items = manifest.commands.len();
                let result = self.read_only_success(
                    vec!["capabilities".to_owned()],
                    to_value(manifest).expect("contract-owned manifest serializes"),
                    returned_items,
                    vec![
                        NextAction {
                            id: "schema.list",
                            argv: vec![
                                self.config.executable.clone(),
                                "schema".to_owned(),
                                "list".to_owned(),
                                "--output".to_owned(),
                                "json".to_owned(),
                            ],
                            side_effect: SideEffect::ReadOnly,
                            retry_safety: RetrySafety::Safe,
                            preconditions: Vec::new(),
                            reason: "Discover every installed machine schema",
                        },
                        NextAction {
                            id: "doctor",
                            argv: vec![
                                self.config.executable.clone(),
                                "doctor".to_owned(),
                                "--output".to_owned(),
                                "json".to_owned(),
                                "--non-interactive".to_owned(),
                            ],
                            side_effect: SideEffect::ReadOnly,
                            retry_safety: RetrySafety::Safe,
                            preconditions: Vec::new(),
                            reason: "Check local prerequisites without contacting a device",
                        },
                    ],
                );
                let human_summary = format!(
                    "Installed CLI contract 1.0 for {} {}: {} commands, {} schemas.",
                    self.config.executable,
                    self.config.cli_version,
                    returned_items,
                    self.contracts.schema_ids().count(),
                );
                self.validated_output(&result, &human_summary, output_mode)
            }
            Some(("schema", schema_matches)) => match schema_matches.subcommand() {
                Some(("list", _)) => {
                    let schemas = self.contracts.schema_ids().collect::<Vec<_>>();
                    let returned_items = schemas.len();
                    let result = self.read_only_success(
                        vec!["schema".to_owned(), "list".to_owned()],
                        serde_json::json!({"schemas": schemas}),
                        returned_items,
                        Vec::new(),
                    );
                    let human_summary =
                        format!("Installed CLI contract: {returned_items} schemas.");
                    self.validated_output(&result, &human_summary, output_mode)
                }
                Some(("show", show_matches)) => {
                    let schema_id = show_matches
                        .get_one::<String>("schema-id")
                        .expect("clap requires a schema identifier");
                    let Some(schema) = self.contracts.schema(schema_id).cloned() else {
                        return self.invalid_invocation_output(
                            vec!["schema".to_owned(), "show".to_owned()],
                            output_mode,
                            true,
                        );
                    };
                    let result = self.read_only_success(
                        vec!["schema".to_owned(), "show".to_owned()],
                        serde_json::json!({
                            "schema_id": schema_id,
                            "schema": schema,
                        }),
                        1,
                        Vec::new(),
                    );
                    let human_summary = format!("Embedded schema: {schema_id}");
                    self.validated_output(&result, &human_summary, output_mode)
                }
                None => command_help(&self.config, "schema"),
                Some(_) => unreachable!("the registry owns every schema command"),
            },
            Some(("doctor", _)) => {
                let report = DoctorReport {
                    checks: vec![
                        DoctorCheck {
                            id: "cli.runtime",
                            status: "passed",
                            message: "The CLI runtime is available",
                        },
                        DoctorCheck {
                            id: "contract.schemas",
                            status: "passed",
                            message: "Every embedded schema is valid and resolves offline",
                        },
                        DoctorCheck {
                            id: "credentials",
                            status: "skipped",
                            message: "Credential adapters are not installed in this slice",
                        },
                        DoctorCheck {
                            id: "device.connection",
                            status: "skipped",
                            message: "Device discovery is not installed in this slice",
                        },
                        DoctorCheck {
                            id: "platform.android_tools",
                            status: "unavailable",
                            message: "The Android platform adapter is not installed",
                        },
                        DoctorCheck {
                            id: "platform.apple_tools",
                            status: "unavailable",
                            message: "The Apple platform adapter is not installed",
                        },
                        DoctorCheck {
                            id: "transport",
                            status: "skipped",
                            message: "Transport adapters are not installed in this slice",
                        },
                    ],
                };
                let returned_items = report.checks.len();
                let result = self.read_only_success(
                    vec!["doctor".to_owned()],
                    to_value(report).expect("contract-owned doctor report serializes"),
                    returned_items,
                    Vec::new(),
                );
                self.validated_output(
                    &result,
                    "Doctor: 2 passed, 3 skipped, 2 unavailable.",
                    output_mode,
                )
            }
            None => root_help(&self.config),
            Some(_) => unreachable!("the registry owns every parsed command"),
        }
    }

    fn read_only_success(
        &self,
        command: Vec<String>,
        data: serde_json::Value,
        returned_items: usize,
        next_actions: Vec<NextAction>,
    ) -> MachineResult {
        let mut context = OutcomeContext::new(Disclosure::complete(returned_items));
        context.next_actions = next_actions;
        let (side_effect, retry_safety) =
            command_annotations(&command).expect("handler command comes from the registry");
        InvocationMetadata::new(command, &[side_effect], retry_safety).complete(
            &self.config.cli_version,
            HandlerOutcome::Succeeded { data, context },
        )
    }

    fn validated_output(
        &self,
        result: &MachineResult,
        human_summary: &str,
        output_mode: OutputMode,
    ) -> ProcessOutput {
        self.render_result(result, human_summary, output_mode, true)
    }

    fn render_result(
        &self,
        result: &MachineResult,
        human_summary: &str,
        output_mode: OutputMode,
        handler_started: bool,
    ) -> ProcessOutput {
        let value = serde_json::to_value(result).expect("contract-owned result serializes");
        self.contracts
            .validate(
                "https://apppilotkit.dev/cli/v1/machine-result.schema.json",
                &value,
            )
            .expect("typed Machine Result conforms to its source schema");
        if result.data.is_some() {
            let data_schema_id = command_result_schema_id(&result.command)
                .expect("a successful handler command declares its result schema in the registry");
            self.contracts
                .validate(data_schema_id, &value["data"])
                .expect("typed command data conforms to its source schema");
        }
        let exit_code = exit_code_for(
            result.status.as_str(),
            result.error.as_ref().map(|error| error.kind),
        );
        match output_mode {
            OutputMode::Human => text_output(
                human_summary,
                exit_code,
                result.status.as_str() != "succeeded",
            ),
            OutputMode::Json => json_output(&value, exit_code),
            OutputMode::Jsonl => self.jsonl_output(&value, exit_code, handler_started),
        }
    }

    fn invalid_invocation_output(
        &self,
        command: Vec<String>,
        output_mode: OutputMode,
        handler_started: bool,
    ) -> ProcessOutput {
        let mut context = OutcomeContext::new(Disclosure::complete(0));
        context.next_actions = vec![NextAction {
            id: "help",
            argv: std::iter::once(self.config.executable.clone())
                .chain(command.iter().cloned())
                .chain(std::iter::once("--help".to_owned()))
                .collect(),
            side_effect: SideEffect::ReadOnly,
            retry_safety: RetrySafety::Safe,
            preconditions: Vec::new(),
            reason: "Read authoritative help for the recognized command",
        }];
        let result = InvocationMetadata::new(
            command,
            &[SideEffect::ReadOnly],
            RetrySafety::Safe,
        )
        .complete(
            &self.config.cli_version,
            HandlerOutcome::Failed {
                error: StructuredError {
                kind: "cli.invalidInvocation",
                message: "Invalid CLI invocation. Use built-in help or capability discovery.",
                retryable: false,
                details: serde_json::Map::new(),
                },
                context,
            },
        );
        self.render_result(
            &result,
            "Invalid CLI invocation. Use built-in help or capability discovery.",
            output_mode,
            handler_started,
        )
    }

    fn jsonl_output(
        &self,
        result: &serde_json::Value,
        exit_code: u8,
        include_started: bool,
    ) -> ProcessOutput {
        let run_id = format!(
            "run-{}-{}",
            std::process::id(),
            self.next_run_id.fetch_add(1, Ordering::Relaxed)
        );
        let started = serde_json::json!({
            "type": "run.started",
            "schema_version": "1.0",
            "cli_version": self.config.cli_version,
            "run_id": run_id,
            "command": result["command"],
            "side_effect": result["side_effect"],
            "retry_safety": result["retry_safety"],
        });
        let terminal_type = match result["status"].as_str() {
            Some("succeeded") => "run.succeeded",
            Some("failed") => "run.failed",
            Some("cancelled") => "run.cancelled",
            _ => unreachable!("Machine Result schema restricts status"),
        };
        let terminal = serde_json::json!({
            "type": terminal_type,
            "schema_version": "1.0",
            "cli_version": self.config.cli_version,
            "run_id": run_id,
            "result": result,
        });
        let events = if include_started {
            vec![started, terminal]
        } else {
            vec![terminal]
        };
        for event in &events {
            self.contracts
                .validate(
                    "https://apppilotkit.dev/cli/v1/jsonl-event.schema.json",
                    event,
                )
                .expect("typed JSONL event conforms to its source schema");
        }
        let mut stdout = Vec::new();
        for event in events {
            stdout
                .extend(serde_json::to_vec(&event).expect("contract-owned JSONL event serializes"));
            stdout.push(b'\n');
        }
        ProcessOutput {
            stdout,
            stderr: Vec::new(),
            exit_code,
        }
    }
}

fn unambiguous_machine_output(arguments: &[OsString]) -> Option<OutputMode> {
    let mut selected = None;
    let mut selectors = 0;
    let mut index = 1;
    while index < arguments.len() {
        let Some(argument) = arguments[index].to_str() else {
            index += 1;
            continue;
        };
        if argument == "--" {
            break;
        }
        let value = if argument == "--output" {
            selectors += 1;
            index += 1;
            arguments.get(index).and_then(|value| value.to_str())
        } else if let Some(value) = argument.strip_prefix("--output=") {
            selectors += 1;
            Some(value)
        } else {
            index += 1;
            continue;
        };
        selected = match value {
            Some("json") => Some(OutputMode::Json),
            Some("jsonl") => Some(OutputMode::Jsonl),
            _ => return None,
        };
        index += 1;
    }
    (selectors == 1).then_some(selected?).or(None)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Human,
    Json,
    Jsonl,
}

impl OutputMode {
    fn from_matches(matches: &clap::ArgMatches) -> Self {
        match matches
            .get_one::<String>("output")
            .expect("clap supplies the output default")
            .as_str()
        {
            "human" => Self::Human,
            "json" => Self::Json,
            "jsonl" => Self::Jsonl,
            _ => unreachable!("clap restricts output values"),
        }
    }
}

fn json_output(value: &impl serde::Serialize, exit_code: u8) -> ProcessOutput {
    let mut stdout = serde_json::to_vec(value).expect("contract-owned result serializes");
    stdout.push(b'\n');
    ProcessOutput {
        stdout,
        stderr: Vec::new(),
        exit_code,
    }
}

fn text_output(value: &str, exit_code: u8, stderr: bool) -> ProcessOutput {
    let mut bytes = value.as_bytes().to_vec();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    if stderr {
        ProcessOutput {
            stdout: Vec::new(),
            stderr: bytes,
            exit_code,
        }
    } else {
        ProcessOutput {
            stdout: bytes,
            stderr: Vec::new(),
            exit_code,
        }
    }
}

fn clap_output(error: clap::Error) -> ProcessOutput {
    let mut output = error.to_string().into_bytes();
    if !output.ends_with(b"\n") {
        output.push(b'\n');
    }
    let exit_code = u8::try_from(error.exit_code()).unwrap_or(2);
    if error.use_stderr() {
        ProcessOutput {
            stdout: Vec::new(),
            stderr: output,
            exit_code,
        }
    } else {
        ProcessOutput {
            stdout: output,
            stderr: Vec::new(),
            exit_code,
        }
    }
}

fn exit_code_for(status: &str, error_kind: Option<&str>) -> u8 {
    if status == "succeeded" {
        return 0;
    }
    if status == "cancelled" {
        return if error_kind == Some("run.cancelled") {
            130
        } else {
            1
        };
    }

    let Some(error_kind) = error_kind else {
        return 1;
    };
    let leaf = error_kind.rsplit('.').next().unwrap_or(error_kind);
    if error_kind == "cli.invalidInvocation" {
        2
    } else if error_kind.starts_with("transport.") || error_kind.starts_with("authentication.") {
        3
    } else if error_kind.starts_with("target.")
        || error_kind.starts_with("app.")
        || matches!(
            leaf,
            "sessionExpired" | "cursorExpired" | "snapshotExpired" | "referenceNotFound"
        )
    {
        4
    } else if error_kind == "action.outcomeUnknown" {
        5
    } else if matches!(
        leaf,
        "parseError"
            | "invalidRequest"
            | "methodNotFound"
            | "invalidParams"
            | "incompatibleProtocol"
            | "capabilityUnavailable"
    ) {
        6
    } else if error_kind.starts_with("artifact.") || error_kind.starts_with("output.") {
        7
    } else {
        1
    }
}

fn root_help(config: &CliConfig) -> ProcessOutput {
    let mut command = command_model(&config.executable, &config.cli_version);
    let mut stdout = Vec::new();
    command
        .write_long_help(&mut stdout)
        .expect("writing help to memory succeeds");
    stdout.push(b'\n');
    ProcessOutput {
        stdout,
        stderr: Vec::new(),
        exit_code: 0,
    }
}

fn command_help(config: &CliConfig, name: &str) -> ProcessOutput {
    let mut root = command_model(&config.executable, &config.cli_version);
    root.build();
    let command = root
        .find_subcommand_mut(name)
        .expect("registry command exists");
    let mut stdout = Vec::new();
    command
        .write_long_help(&mut stdout)
        .expect("writing help to memory succeeds");
    stdout.push(b'\n');
    ProcessOutput {
        stdout,
        stderr: Vec::new(),
        exit_code: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{CliConfig, CliCore, OutputMode, exit_code_for};
    use crate::registry::capability_manifest;
    use crate::result::{
        CancellationResolution, Disclosure, HandlerOutcome, InvocationMetadata, OutcomeContext,
        RetrySafety, SideEffect, StructuredError,
    };
    use serde_json::Value;

    #[test]
    fn exit_categories_are_derived_from_status_and_authoritative_error_kind() {
        let cases = [
            ("succeeded", None, 0),
            ("failed", Some("cli.internalError"), 1),
            ("failed", Some("cli.invalidInvocation"), 2),
            ("failed", Some("transport.authenticationRequired"), 3),
            ("failed", Some("target.selectionRequired"), 4),
            ("failed", Some("ui.snapshotExpired"), 4),
            ("failed", Some("action.outcomeUnknown"), 5),
            ("failed", Some("protocol.incompatibleProtocol"), 6),
            ("failed", Some("artifact.alreadyExists"), 7),
            ("cancelled", Some("run.cancelled"), 130),
        ];

        for (status, kind, expected) in cases {
            assert_eq!(exit_code_for(status, kind), expected, "{status} {kind:?}");
        }
    }

    #[test]
    fn capability_error_catalog_and_renderer_share_the_same_exit_mapping() {
        let manifest = capability_manifest("fixture-cli", "0.1.0");
        for error in manifest.error_kinds {
            let status = if error.kind == "run.cancelled" {
                "cancelled"
            } else {
                "failed"
            };
            assert_eq!(
                exit_code_for(status, Some(error.kind)),
                error.exit_code,
                "{}",
                error.kind
            );
        }
    }

    #[test]
    fn ambiguous_mutation_takes_precedence_over_interrupted_cancellation() {
        assert_eq!(exit_code_for("failed", Some("action.outcomeUnknown")), 5);
        assert_eq!(exit_code_for("cancelled", Some("run.cancelled")), 130);
    }

    #[test]
    fn typed_failures_render_every_frozen_non_ambiguous_exit_category() {
        let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0")).expect("CLI initializes");
        for (kind, side_effect, retry_safety, expected_exit) in [
            (
                "cli.internalError",
                SideEffect::ReadOnly,
                RetrySafety::Safe,
                1,
            ),
            (
                "cli.invalidInvocation",
                SideEffect::ReadOnly,
                RetrySafety::Safe,
                2,
            ),
            (
                "transport.authenticationRequired",
                SideEffect::ReadOnly,
                RetrySafety::Safe,
                3,
            ),
            (
                "target.selectionRequired",
                SideEffect::ReadOnly,
                RetrySafety::Safe,
                4,
            ),
            (
                "incompatibleProtocol",
                SideEffect::ReadOnly,
                RetrySafety::Safe,
                6,
            ),
            (
                "artifact.alreadyExists",
                SideEffect::LocalWrite,
                RetrySafety::RequiresArtifactConflictPolicy,
                7,
            ),
        ] {
            let result =
                InvocationMetadata::new(vec!["doctor".to_owned()], &[side_effect], retry_safety)
                    .complete(
                        "0.1.0",
                        HandlerOutcome::Failed {
                            error: StructuredError {
                                kind,
                                message: "The operation failed",
                                retryable: false,
                                details: serde_json::Map::new(),
                            },
                            context: OutcomeContext::new(Disclosure::complete(0)),
                        },
                    );
            let output =
                core.render_result(&result, "The operation failed", OutputMode::Json, true);
            let rendered: Value =
                serde_json::from_slice(&output.stdout).expect("renderer emits bare JSON");

            assert_eq!(output.exit_code, expected_exit, "{kind}");
            assert_eq!(rendered["error"]["kind"], kind);
        }
    }

    #[test]
    fn definitive_cancellation_renders_exit_130_and_one_matching_jsonl_terminal() {
        let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0")).expect("CLI initializes");
        let result = InvocationMetadata::new(
            vec!["doctor".to_owned()],
            &[SideEffect::ReadOnly],
            RetrySafety::Safe,
        )
        .complete(
            "0.1.0",
            HandlerOutcome::Cancelled {
                resolution: CancellationResolution::DefinitivelyCancelled,
                context: OutcomeContext::new(Disclosure::complete(0)),
            },
        );
        let output = core.render_result(
            &result,
            "The operation was cancelled",
            OutputMode::Jsonl,
            true,
        );
        let events = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).expect("JSONL event"))
            .collect::<Vec<_>>();

        assert_eq!(output.exit_code, 130);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "run.started");
        assert_eq!(events[1]["type"], "run.cancelled");
        assert_eq!(events[1]["result"]["status"], "cancelled");
        assert_eq!(events[1]["result"]["error"]["kind"], "run.cancelled");
        assert_eq!(events[0]["run_id"], events[1]["run_id"]);
    }

    #[test]
    fn ambiguous_action_fixtures_render_as_non_replayable_exit_five_failures() {
        let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0")).expect("CLI initializes");
        for (operation, source) in [
            (
                "tap",
                include_str!("../../../contracts/v1/fixtures/valid/outcome-unknown-tap.json"),
            ),
            (
                "swipe",
                include_str!("../../../contracts/v1/fixtures/valid/outcome-unknown-swipe.json"),
            ),
            (
                "type",
                include_str!("../../../contracts/v1/fixtures/valid/outcome-unknown-type.json"),
            ),
        ] {
            let fixture: Value = serde_json::from_str(source).expect("checked-in fixture JSON");
            let result = InvocationMetadata::new(
                vec!["action".to_owned(), operation.to_owned()],
                &[SideEffect::AppMutation],
                RetrySafety::Safe,
            )
            .complete(
                "0.1.0",
                HandlerOutcome::Cancelled {
                    resolution: CancellationResolution::MutationMayHaveExecuted { operation },
                    context: OutcomeContext::new(Disclosure::complete(0)),
                },
            );
            let output = core.render_result(
                &result,
                "The mutation may have executed; do not replay it.",
                OutputMode::Json,
                true,
            );
            let rendered: Value =
                serde_json::from_slice(&output.stdout).expect("renderer emits bare JSON");

            assert_eq!(output.exit_code, 5);
            assert!(output.stderr.is_empty());
            assert_eq!(rendered, fixture);
            assert_eq!(rendered["status"], "failed");
            assert_eq!(rendered["error"]["kind"], "action.outcomeUnknown");
            assert_eq!(rendered["error"]["retryable"], false);
            assert_eq!(rendered["retry_safety"], "unsafe_after_ambiguous_result");
            assert!(
                rendered["next_actions"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
                "this slice has no installed read-only recovery command"
            );
        }
    }
}
