use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalStatus {
    Succeeded,
    Failed,
    Cancelled,
}

impl TerminalStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SideEffect {
    ReadOnly,
    LocalWrite,
    AppMutation,
    DeviceMutation,
}

impl SideEffect {
    pub(crate) const ALL: [Self; 4] = [
        Self::AppMutation,
        Self::DeviceMutation,
        Self::LocalWrite,
        Self::ReadOnly,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::LocalWrite => "local_write",
            Self::AppMutation => "app_mutation",
            Self::DeviceMutation => "device_mutation",
        }
    }

    fn precedence(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::LocalWrite => 1,
            Self::AppMutation => 2,
            Self::DeviceMutation => 3,
        }
    }

    fn primary(effects: &[Self]) -> Self {
        effects
            .iter()
            .copied()
            .max_by_key(|effect| effect.precedence())
            .expect("invocation metadata declares at least one side effect")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetrySafety {
    Safe,
    RequiresIdempotencyKey,
    RequiresArtifactConflictPolicy,
    UnsafeAfterAmbiguousResult,
}

impl RetrySafety {
    pub(crate) const ALL: [Self; 4] = [
        Self::RequiresArtifactConflictPolicy,
        Self::RequiresIdempotencyKey,
        Self::Safe,
        Self::UnsafeAfterAmbiguousResult,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::RequiresIdempotencyKey => "requires_idempotency_key",
            Self::RequiresArtifactConflictPolicy => "requires_artifact_conflict_policy",
            Self::UnsafeAfterAmbiguousResult => "unsafe_after_ambiguous_result",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct MachineResult {
    pub schema_version: &'static str,
    pub cli_version: String,
    pub status: TerminalStatus,
    pub command: Vec<String>,
    pub side_effect: SideEffect,
    pub retry_safety: RetrySafety,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<StructuredError>,
    pub disclosure: Disclosure,
    pub artifacts: Vec<Artifact>,
    pub next_actions: Vec<NextAction>,
}

#[derive(Debug, Serialize)]
pub(crate) struct StructuredError {
    pub kind: &'static str,
    pub message: &'static str,
    pub retryable: bool,
    pub details: serde_json::Map<String, Value>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Disclosure {
    pub truncated: bool,
    pub returned_items: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_limits: Option<AppliedLimits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasons: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl Disclosure {
    pub(crate) fn complete(returned_items: usize) -> Self {
        Self {
            truncated: false,
            returned_items,
            applied_limits: None,
            reasons: None,
            next_cursor: None,
        }
    }

    #[allow(dead_code)] // No installed command consumes a cursor in this discovery-only slice.
    pub(crate) fn truncated(
        returned_items: usize,
        applied_limits: AppliedLimits,
        reasons: Vec<&'static str>,
        next_cursor: impl Into<String>,
    ) -> Self {
        Self {
            truncated: true,
            returned_items,
            applied_limits: Some(applied_limits),
            reasons: Some(reasons),
            next_cursor: Some(next_cursor.into()),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct AppliedLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Artifact {
    pub id: String,
    pub kind: String,
    pub path: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub digest: ArtifactDigest,
    pub sensitive: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ArtifactDigest {
    pub algorithm: &'static str,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct NextAction {
    pub id: &'static str,
    pub argv: Vec<String>,
    pub side_effect: SideEffect,
    pub retry_safety: RetrySafety,
    pub preconditions: Vec<&'static str>,
    pub reason: &'static str,
}

#[derive(Debug)]
pub(crate) struct OutcomeContext {
    pub disclosure: Disclosure,
    pub artifacts: Vec<Artifact>,
    pub next_actions: Vec<NextAction>,
}

impl OutcomeContext {
    pub(crate) fn new(disclosure: Disclosure) -> Self {
        Self {
            disclosure,
            artifacts: Vec::new(),
            next_actions: Vec::new(),
        }
    }
}

#[allow(dead_code)] // Action execution is out of scope; renderer tests freeze these future inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CancellationResolution {
    DidNotExecute,
    DefinitivelyCancelled,
    MutationMayHaveExecuted { operation: &'static str },
}

#[allow(dead_code)] // No installed command can be cancelled in this discovery-only slice.
#[derive(Debug)]
pub(crate) enum HandlerOutcome {
    Succeeded {
        data: Value,
        context: OutcomeContext,
    },
    Failed {
        error: StructuredError,
        context: OutcomeContext,
    },
    Cancelled {
        resolution: CancellationResolution,
        context: OutcomeContext,
    },
}

#[derive(Debug)]
pub(crate) struct InvocationMetadata {
    command: Vec<String>,
    effects: Vec<SideEffect>,
    retry_safety: RetrySafety,
}

impl InvocationMetadata {
    pub(crate) fn new(
        command: Vec<String>,
        effects: &[SideEffect],
        retry_safety: RetrySafety,
    ) -> Self {
        assert!(
            !effects.is_empty(),
            "an invocation declares its side effects"
        );
        Self {
            command,
            effects: effects.to_vec(),
            retry_safety,
        }
    }

    pub(crate) fn complete(self, cli_version: &str, outcome: HandlerOutcome) -> MachineResult {
        let (status, retry_safety, data, error, context) = match outcome {
            HandlerOutcome::Succeeded { data, context } => (
                TerminalStatus::Succeeded,
                self.retry_safety,
                Some(data),
                None,
                context,
            ),
            HandlerOutcome::Failed {
                mut error,
                mut context,
            } if error.kind == "action.outcomeUnknown" => {
                error.retryable = false;
                context.next_actions.clear();
                (
                    TerminalStatus::Failed,
                    RetrySafety::UnsafeAfterAmbiguousResult,
                    None,
                    Some(error),
                    context,
                )
            }
            HandlerOutcome::Failed { error, context } => (
                TerminalStatus::Failed,
                self.retry_safety,
                None,
                Some(error),
                context,
            ),
            HandlerOutcome::Cancelled {
                resolution: CancellationResolution::MutationMayHaveExecuted { operation },
                mut context,
            } => {
                context.next_actions.clear();
                (
                    TerminalStatus::Failed,
                    RetrySafety::UnsafeAfterAmbiguousResult,
                    None,
                    Some(StructuredError {
                        kind: "action.outcomeUnknown",
                        message: "The mutation may have executed; do not replay it.",
                        retryable: false,
                        details: serde_json::Map::from_iter([(
                            "operation".to_owned(),
                            Value::String(operation.to_owned()),
                        )]),
                    }),
                    context,
                )
            }
            HandlerOutcome::Cancelled {
                resolution:
                    CancellationResolution::DidNotExecute
                    | CancellationResolution::DefinitivelyCancelled,
                context,
            } => (
                TerminalStatus::Cancelled,
                self.retry_safety,
                None,
                Some(StructuredError {
                    kind: "run.cancelled",
                    message: "The operation was definitively cancelled after bounded cleanup.",
                    retryable: self.retry_safety == RetrySafety::Safe,
                    details: serde_json::Map::new(),
                }),
                context,
            ),
        };
        let mut effects = self.effects;
        if !context.artifacts.is_empty() {
            effects.push(SideEffect::LocalWrite);
        }
        let side_effect = SideEffect::primary(&effects);
        MachineResult {
            schema_version: "1.0",
            cli_version: cli_version.to_owned(),
            status,
            command: self.command,
            side_effect,
            retry_safety,
            data,
            error,
            disclosure: context.disclosure,
            artifacts: context.artifacts,
            next_actions: context.next_actions,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DoctorCheck {
    pub id: &'static str,
    pub status: &'static str,
    pub message: &'static str,
}

#[cfg(test)]
mod tests {
    use super::{
        AppliedLimits, Artifact, ArtifactDigest, CancellationResolution, Disclosure,
        HandlerOutcome, InvocationMetadata, OutcomeContext, RetrySafety, SideEffect,
    };

    #[test]
    fn side_effect_precedence_keeps_mutation_primary_when_it_writes_an_artifact() {
        assert_eq!(
            SideEffect::primary(&[SideEffect::LocalWrite, SideEffect::AppMutation]),
            SideEffect::AppMutation
        );
        assert_eq!(
            SideEffect::primary(&[SideEffect::AppMutation, SideEffect::DeviceMutation]),
            SideEffect::DeviceMutation
        );
        let invocation = InvocationMetadata::new(
            vec!["action".to_owned(), "tap".to_owned()],
            &[SideEffect::AppMutation],
            RetrySafety::Safe,
        );
        let mut context = OutcomeContext::new(Disclosure::complete(0));
        context.artifacts.push(Artifact {
            id: "artifact_action-log".to_owned(),
            kind: "action_log".to_owned(),
            path: "/tmp/action.json".to_owned(),
            media_type: "application/json".to_owned(),
            size_bytes: 2,
            digest: ArtifactDigest {
                algorithm: "sha-256",
                value: "0".repeat(64),
            },
            sensitive: false,
        });
        let result = invocation.complete(
            "0.1.0",
            HandlerOutcome::Succeeded {
                data: serde_json::json!({}),
                context,
            },
        );

        assert_eq!(result.side_effect, SideEffect::AppMutation);
        assert_eq!(result.artifacts.len(), 1);
    }

    #[test]
    fn interrupted_mutation_ambiguity_overrides_cancellation() {
        let invocation = InvocationMetadata::new(
            vec!["action".to_owned(), "tap".to_owned()],
            &[SideEffect::AppMutation],
            RetrySafety::Safe,
        );
        let result = invocation.complete(
            "0.1.0",
            HandlerOutcome::Cancelled {
                resolution: CancellationResolution::MutationMayHaveExecuted { operation: "tap" },
                context: OutcomeContext::new(Disclosure::complete(0)),
            },
        );

        assert_eq!(result.status.as_str(), "failed");
        assert_eq!(result.error.as_ref().unwrap().kind, "action.outcomeUnknown");
        assert!(!result.error.as_ref().unwrap().retryable);
        assert_eq!(result.retry_safety, RetrySafety::UnsafeAfterAmbiguousResult);
        assert!(result.next_actions.is_empty());
    }

    #[test]
    fn definitive_cancellation_is_retryable_only_for_safe_invocations() {
        for resolution in [
            CancellationResolution::DidNotExecute,
            CancellationResolution::DefinitivelyCancelled,
        ] {
            for (retry_safety, expected_retryable) in [
                (RetrySafety::Safe, true),
                (RetrySafety::RequiresIdempotencyKey, false),
                (RetrySafety::RequiresArtifactConflictPolicy, false),
            ] {
                let invocation = InvocationMetadata::new(
                    vec!["doctor".to_owned()],
                    &[SideEffect::ReadOnly],
                    retry_safety,
                );
                let result = invocation.complete(
                    "0.1.0",
                    HandlerOutcome::Cancelled {
                        resolution,
                        context: OutcomeContext::new(Disclosure::complete(0)),
                    },
                );

                assert_eq!(result.status.as_str(), "cancelled");
                assert_eq!(result.error.as_ref().unwrap().kind, "run.cancelled");
                assert_eq!(result.error.as_ref().unwrap().retryable, expected_retryable);
            }
        }
    }

    #[test]
    fn typed_disclosure_preserves_truncation_evidence_without_inventing_an_action() {
        let disclosure = Disclosure::truncated(
            100,
            AppliedLimits {
                max_items: Some(100),
                max_bytes: None,
            },
            vec!["max_items"],
            "cursor_opaque-1",
        );
        let result = InvocationMetadata::new(
            vec!["capabilities".to_owned()],
            &[SideEffect::ReadOnly],
            RetrySafety::Safe,
        )
        .complete(
            "0.1.0",
            HandlerOutcome::Succeeded {
                data: serde_json::json!({}),
                context: OutcomeContext::new(disclosure),
            },
        );

        let value = serde_json::to_value(&result).expect("typed result serializes");
        assert_eq!(value["disclosure"]["truncated"], true);
        assert_eq!(value["disclosure"]["next_cursor"], "cursor_opaque-1");
        assert!(result.next_actions.is_empty());
    }
}
