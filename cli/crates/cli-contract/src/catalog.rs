//! Semantic Catalog command runtime.

use crate::protocol::ProtocolContractCatalog;
use crate::result::{
    AppliedLimits, Disclosure, HandlerOutcome, NextAction, OutcomeContext, RetrySafety, SideEffect,
    StructuredError,
};
use clap::ArgMatches;
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const INPUT_JSON_MAX_BYTES: usize = 64 * 1024;
const SEMANTIC_CATALOG: &str = "semantic.catalog";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionSelector<'a> {
    pub session: Option<&'a str>,
    pub target: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCandidate {
    pub session_id: String,
    pub target_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedProtocolSession {
    pub session_id: String,
    pub generation: u64,
    pub target_id: String,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub capabilities: Vec<String>,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_page_items: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogSelectError {
    SessionSelectionRequired { candidates: Vec<SessionCandidate> },
    TargetSelectionRequired { candidates: Vec<SessionCandidate> },
    SessionExpired,
    AuthenticationRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogDispatchPhase {
    PreDispatch,
    PostDispatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogExchangeFailure {
    Timeout,
    EndOfStream,
    SessionExpired,
    AuthenticationRequired,
    TransportInternal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogExchangeError {
    pub phase: CatalogDispatchPhase,
    pub failure: CatalogExchangeFailure,
}

impl CatalogExchangeError {
    #[must_use]
    pub const fn pre_dispatch(failure: CatalogExchangeFailure) -> Self {
        Self {
            phase: CatalogDispatchPhase::PreDispatch,
            failure,
        }
    }

    #[must_use]
    pub const fn post_dispatch(failure: CatalogExchangeFailure) -> Self {
        Self {
            phase: CatalogDispatchPhase::PostDispatch,
            failure,
        }
    }
}

pub trait CatalogRuntime: Send + Sync {
    fn select(
        &self,
        selector: SessionSelector<'_>,
    ) -> Result<OpenedProtocolSession, CatalogSelectError>;
    fn exchange(
        &self,
        session: &OpenedProtocolSession,
        request: &Value,
    ) -> Result<Vec<u8>, CatalogExchangeError>;
}

#[derive(Debug, Default)]
pub struct UnconfiguredCatalogRuntime;

impl CatalogRuntime for UnconfiguredCatalogRuntime {
    fn select(
        &self,
        _selector: SessionSelector<'_>,
    ) -> Result<OpenedProtocolSession, CatalogSelectError> {
        Err(CatalogSelectError::SessionSelectionRequired {
            candidates: Vec::new(),
        })
    }

    fn exchange(
        &self,
        _session: &OpenedProtocolSession,
        _request: &Value,
    ) -> Result<Vec<u8>, CatalogExchangeError> {
        Err(CatalogExchangeError::pre_dispatch(
            CatalogExchangeFailure::TransportInternal,
        ))
    }
}

type Responder = Arc<
    dyn Fn(&OpenedProtocolSession, &Value) -> Result<Vec<u8>, CatalogExchangeError> + Send + Sync,
>;

#[derive(Default)]
struct FakeCatalogState {
    sessions: Mutex<Vec<OpenedProtocolSession>>,
    exchanges: Mutex<Vec<Value>>,
    responder: Mutex<Option<Responder>>,
}

#[derive(Clone, Default)]
pub struct FakeCatalogRuntime {
    state: Arc<FakeCatalogState>,
}

impl FakeCatalogRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_session(&self, session: OpenedProtocolSession) {
        self.state
            .sessions
            .lock()
            .expect("fake catalog runtime lock")
            .push(session);
    }

    pub fn set_responder<F>(&self, responder: F)
    where
        F: Fn(&OpenedProtocolSession, &Value) -> Result<Value, CatalogExchangeError>
            + Send
            + Sync
            + 'static,
    {
        self.set_wire_responder(move |session, request| {
            responder(session, request).and_then(|response| {
                serde_json::to_vec(&response).map_err(|_| {
                    CatalogExchangeError::post_dispatch(CatalogExchangeFailure::TransportInternal)
                })
            })
        });
    }

    pub fn set_wire_responder<F>(&self, responder: F)
    where
        F: Fn(&OpenedProtocolSession, &Value) -> Result<Vec<u8>, CatalogExchangeError>
            + Send
            + Sync
            + 'static,
    {
        *self
            .state
            .responder
            .lock()
            .expect("fake catalog runtime lock") = Some(Arc::new(responder));
    }

    #[must_use]
    pub fn exchange_requests(&self) -> Vec<Value> {
        self.state
            .exchanges
            .lock()
            .expect("fake catalog runtime lock")
            .clone()
    }
}

impl CatalogRuntime for FakeCatalogRuntime {
    fn select(
        &self,
        selector: SessionSelector<'_>,
    ) -> Result<OpenedProtocolSession, CatalogSelectError> {
        let sessions = self
            .state
            .sessions
            .lock()
            .expect("fake catalog runtime lock");
        let mut matched: Vec<&OpenedProtocolSession> = sessions.iter().collect();
        if let Some(session_id) = selector.session {
            matched.retain(|session| session.session_id == session_id);
            if matched.is_empty() {
                return Err(CatalogSelectError::SessionExpired);
            }
        }
        if let Some(target_id) = selector.target {
            let targeted = matched
                .iter()
                .copied()
                .filter(|session| session.target_id == target_id)
                .collect::<Vec<_>>();
            if targeted.is_empty() {
                return Err(CatalogSelectError::TargetSelectionRequired {
                    candidates: candidates(matched),
                });
            }
            matched = targeted;
        }
        match matched.len() {
            0 => Err(CatalogSelectError::SessionSelectionRequired {
                candidates: Vec::new(),
            }),
            1 => Ok(matched[0].clone()),
            _ if selector.session.is_some() => Err(CatalogSelectError::TargetSelectionRequired {
                candidates: candidates(matched),
            }),
            _ => Err(CatalogSelectError::SessionSelectionRequired {
                candidates: candidates(matched),
            }),
        }
    }

    fn exchange(
        &self,
        session: &OpenedProtocolSession,
        request: &Value,
    ) -> Result<Vec<u8>, CatalogExchangeError> {
        self.state
            .exchanges
            .lock()
            .expect("fake catalog runtime lock")
            .push(request.clone());
        match self
            .state
            .responder
            .lock()
            .expect("fake catalog runtime lock")
            .as_ref()
        {
            Some(responder) => responder(session, request),
            None => Err(CatalogExchangeError::pre_dispatch(
                CatalogExchangeFailure::TransportInternal,
            )),
        }
    }
}

fn candidates(sessions: Vec<&OpenedProtocolSession>) -> Vec<SessionCandidate> {
    let mut candidates = sessions
        .into_iter()
        .map(|session| SessionCandidate {
            session_id: session.session_id.clone(),
            target_id: session.target_id.clone(),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.session_id
            .cmp(&right.session_id)
            .then(left.target_id.cmp(&right.target_id))
    });
    candidates
}

pub(crate) struct CatalogOutput {
    pub command: Vec<String>,
    pub outcome: HandlerOutcome,
    pub human_summary: String,
}

pub(crate) fn run(
    executable: &str,
    subcommand: &str,
    matches: &ArgMatches,
    runtime: &dyn CatalogRuntime,
    protocol_contracts: &ProtocolContractCatalog,
    request_id: &AtomicU64,
) -> CatalogOutput {
    let command = vec!["catalog".to_owned(), subcommand.to_owned()];
    match dispatch(
        executable,
        subcommand,
        matches,
        runtime,
        protocol_contracts,
        request_id,
    ) {
        Ok(output) => output,
        Err(failure) => failure.into_output(command),
    }
}

struct LocalFailure {
    error: StructuredError,
    next_actions: Vec<NextAction>,
}

impl LocalFailure {
    fn into_output(self, command: Vec<String>) -> CatalogOutput {
        let message = self.error.message;
        let mut context = OutcomeContext::new(Disclosure::complete(0));
        context.next_actions = self.next_actions;
        CatalogOutput {
            command,
            outcome: HandlerOutcome::Failed {
                error: self.error,
                context,
            },
            human_summary: message.to_owned(),
        }
    }
}

fn dispatch(
    executable: &str,
    subcommand: &str,
    matches: &ArgMatches,
    runtime: &dyn CatalogRuntime,
    protocol_contracts: &ProtocolContractCatalog,
    request_id: &AtomicU64,
) -> Result<CatalogOutput, LocalFailure> {
    let prepared = prepare(subcommand, matches)?;
    let session = select_session(executable, matches, runtime)?;
    negotiate(&session)?;
    let request = encode_request(&session, request_id, &prepared)?;
    let encoded = serde_json::to_vec(&request).expect("catalog request serializes");
    if encoded.len() > session.max_request_bytes {
        return Err(LocalFailure {
            error: structured_error(
                "resourceExhausted",
                "A safe bounded result cannot be produced.",
                false,
                details_from([
                    ("limit", Value::from(session.max_request_bytes as u64)),
                    ("actual_bytes", Value::from(encoded.len() as u64)),
                ]),
            ),
            next_actions: discovery_actions(executable),
        });
    }
    if protocol_contracts
        .validate_request(prepared.method, &request)
        .is_err()
    {
        return Err(internal_error());
    }
    match runtime.exchange(&session, &request) {
        Ok(response_bytes) => {
            let response = validate_response(
                &session,
                &request,
                &prepared,
                response_bytes,
                protocol_contracts,
            );
            match response {
                Ok(response) => map_response(executable, subcommand, &session, &prepared, response),
                Err(_) if subcommand == "invoke" => Ok(outcome_unknown(
                    executable,
                    &session,
                    prepared.capability.as_deref(),
                    prepared.declaration_revision,
                )),
                Err(failure) => Err(failure),
            }
        }
        Err(CatalogExchangeError {
            phase: CatalogDispatchPhase::PostDispatch,
            ..
        }) if subcommand == "invoke" => Ok(outcome_unknown(
            executable,
            &session,
            prepared.capability.as_deref(),
            prepared.declaration_revision,
        )),
        Err(CatalogExchangeError {
            failure: CatalogExchangeFailure::Timeout,
            ..
        }) => Err(LocalFailure {
            error: structured_error(
                "timeout",
                "The operation exceeded its deadline.",
                true,
                Map::new(),
            ),
            next_actions: inspect_actions(
                executable,
                &session,
                prepared.capability.as_deref(),
                prepared.declaration_revision,
            ),
        }),
        Err(CatalogExchangeError {
            failure: CatalogExchangeFailure::SessionExpired,
            ..
        }) => Err(select_error(executable, CatalogSelectError::SessionExpired)),
        Err(CatalogExchangeError {
            failure: CatalogExchangeFailure::AuthenticationRequired,
            ..
        }) => Err(select_error(
            executable,
            CatalogSelectError::AuthenticationRequired,
        )),
        Err(CatalogExchangeError {
            failure: CatalogExchangeFailure::TransportInternal | CatalogExchangeFailure::EndOfStream,
            ..
        }) => Err(LocalFailure {
            error: structured_error(
                "cli.internalError",
                "The CLI runtime failed unexpectedly.",
                false,
                Map::new(),
            ),
            next_actions: discovery_actions(executable),
        }),
    }
}

struct PreparedRequest {
    method: &'static str,
    params: Value,
    capability: Option<String>,
    declaration_revision: Option<u64>,
}

fn prepare(subcommand: &str, matches: &ArgMatches) -> Result<PreparedRequest, LocalFailure> {
    match subcommand {
        "list" => prepare_list(matches),
        "show" => prepare_show(matches),
        "schema" => prepare_schema(matches),
        "query" => prepare_query(matches),
        "invoke" => prepare_invoke(matches),
        _ => unreachable!("the registry owns every catalog command"),
    }
}

fn prepare_list(matches: &ArgMatches) -> Result<PreparedRequest, LocalFailure> {
    let cursor = optional_flag(matches, "cursor");
    let max_items = optional_flag(matches, "max-items");
    let max_bytes = optional_flag(matches, "max-bytes");
    if cursor.is_some() && (max_items.is_some() || max_bytes.is_some()) {
        return Err(invalid_invocation("cursor"));
    }
    let params = if let Some(cursor) = cursor {
        if !(1..=4096).contains(&cursor.chars().count()) {
            return Err(invalid_invocation("cursor"));
        }
        serde_json::json!({ "cursor": cursor })
    } else {
        let mut limits = Map::new();
        if let Some(max_items) = max_items {
            limits.insert(
                "maxItems".to_owned(),
                Value::from(parse_bound(max_items, 1, 10_000, "maxItems")?),
            );
        }
        if let Some(max_bytes) = max_bytes {
            limits.insert(
                "maxBytes".to_owned(),
                Value::from(parse_bound(max_bytes, 1024, 67_108_864, "maxBytes")?),
            );
        }
        if limits.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::json!({ "limits": limits })
        }
    };
    Ok(PreparedRequest {
        method: "semantic.list",
        params,
        capability: None,
        declaration_revision: None,
    })
}

fn prepare_show(matches: &ArgMatches) -> Result<PreparedRequest, LocalFailure> {
    let capability = parse_capability(required_flag(matches, "capability")?)?;
    let declaration_revision = parse_revision(required_flag(matches, "declaration-revision")?)?;
    Ok(PreparedRequest {
        method: "semantic.show",
        params: serde_json::json!({
            "capability": capability,
            "declarationRevision": declaration_revision,
        }),
        capability: Some(capability),
        declaration_revision: Some(declaration_revision),
    })
}

fn prepare_schema(matches: &ArgMatches) -> Result<PreparedRequest, LocalFailure> {
    let capability = parse_capability(required_flag(matches, "capability")?)?;
    let declaration_revision = parse_revision(required_flag(matches, "declaration-revision")?)?;
    let schema = parse_handle(
        required_flag(matches, "schema-id")?,
        required_flag(matches, "schema-revision")?,
        required_flag(matches, "schema-digest")?,
    )?;
    Ok(PreparedRequest {
        method: "semantic.schema",
        params: serde_json::json!({
            "capability": capability,
            "declarationRevision": declaration_revision,
            "schema": schema,
        }),
        capability: Some(capability),
        declaration_revision: Some(declaration_revision),
    })
}

fn prepare_query(matches: &ArgMatches) -> Result<PreparedRequest, LocalFailure> {
    let capability = parse_capability(required_flag(matches, "capability")?)?;
    let declaration_revision = parse_revision(required_flag(matches, "declaration-revision")?)?;
    let value_schema = parse_handle(
        required_flag(matches, "value-schema-id")?,
        required_flag(matches, "value-schema-revision")?,
        required_flag(matches, "value-schema-digest")?,
    )?;
    let input_id = optional_flag(matches, "input-schema-id");
    let input_revision = optional_flag(matches, "input-schema-revision");
    let input_digest = optional_flag(matches, "input-schema-digest");
    let input = optional_flag(matches, "input");
    let has_schema = input_id.is_some() || input_revision.is_some() || input_digest.is_some();
    let has_input = input.is_some();
    if has_schema != has_input
        || (has_schema
            && (input_id.is_none() || input_revision.is_none() || input_digest.is_none()))
    {
        return Err(invalid_invocation(if has_input {
            "inputSchema"
        } else {
            "input"
        }));
    }
    let mut params = serde_json::json!({
        "capability": capability,
        "declarationRevision": declaration_revision,
        "valueSchema": value_schema,
    });
    if let (Some(id), Some(revision), Some(digest), Some(input)) =
        (input_id, input_revision, input_digest, input)
    {
        params["inputSchema"] = parse_handle(id, revision, digest)?;
        params["input"] = parse_input_json(input)?;
    }
    Ok(PreparedRequest {
        method: "semantic.query",
        params,
        capability: Some(capability),
        declaration_revision: Some(declaration_revision),
    })
}

fn prepare_invoke(matches: &ArgMatches) -> Result<PreparedRequest, LocalFailure> {
    let capability = parse_capability(required_flag(matches, "capability")?)?;
    let declaration_revision = parse_revision(required_flag(matches, "declaration-revision")?)?;
    let input_schema = parse_handle(
        required_flag(matches, "input-schema-id")?,
        required_flag(matches, "input-schema-revision")?,
        required_flag(matches, "input-schema-digest")?,
    )?;
    let input = parse_input_json(required_flag(matches, "input")?)?;
    let mut params = serde_json::json!({
        "capability": capability,
        "declarationRevision": declaration_revision,
        "inputSchema": input_schema,
        "input": input,
    });
    if let Some(grant) = optional_flag(matches, "authorization-grant") {
        if grant.is_empty() || grant.chars().count() > 256 {
            return Err(invalid_invocation("authorizationGrant"));
        }
        params["authorizationGrant"] = Value::String(grant.to_owned());
    }
    Ok(PreparedRequest {
        method: "semantic.invoke",
        params,
        capability: Some(capability),
        declaration_revision: Some(declaration_revision),
    })
}

fn select_session(
    executable: &str,
    matches: &ArgMatches,
    runtime: &dyn CatalogRuntime,
) -> Result<OpenedProtocolSession, LocalFailure> {
    let session = optional_flag(matches, "session");
    let target = optional_flag(matches, "target");
    if let Some(session) = session
        && !is_session_id(session)
    {
        return Err(invalid_invocation("session"));
    }
    if let Some(target) = target
        && !is_target_id(target)
    {
        return Err(invalid_invocation("target"));
    }
    match runtime.select(SessionSelector { session, target }) {
        Ok(session) => {
            validate_opened_session(&session)?;
            Ok(session)
        }
        Err(error) => Err(select_error(executable, error)),
    }
}

fn validate_opened_session(session: &OpenedProtocolSession) -> Result<(), LocalFailure> {
    let mut capabilities = HashSet::with_capacity(session.capabilities.len());
    if !is_session_id(&session.session_id)
        || session.generation == 0
        || !is_target_id(&session.target_id)
        || session.capabilities.iter().any(|capability| {
            !is_protocol_capability(capability) || !capabilities.insert(capability)
        })
        || !(1024..=16_777_216).contains(&session.max_request_bytes)
        || !(1024..=67_108_864).contains(&session.max_response_bytes)
        || !(1..=10_000).contains(&session.max_page_items)
    {
        return Err(peer_contract_failure());
    }
    Ok(())
}

fn negotiate(session: &OpenedProtocolSession) -> Result<(), LocalFailure> {
    if session.protocol_major != 1 || session.protocol_minor != 2 {
        return Err(LocalFailure {
            error: structured_error(
                "incompatibleProtocol",
                "No compatible protocol version exists.",
                false,
                Map::new(),
            ),
            next_actions: Vec::new(),
        });
    }
    if !session
        .capabilities
        .iter()
        .any(|capability| capability == SEMANTIC_CATALOG)
    {
        return Err(LocalFailure {
            error: structured_error(
                "capabilityUnavailable",
                "Capability unavailable.",
                false,
                details_from([("method", Value::String("semantic.catalog".to_owned()))]),
            ),
            next_actions: Vec::new(),
        });
    }
    Ok(())
}

fn encode_request(
    session: &OpenedProtocolSession,
    request_id: &AtomicU64,
    prepared: &PreparedRequest,
) -> Result<Value, LocalFailure> {
    let id = format!("catalog-{}", request_id.fetch_add(1, Ordering::Relaxed));
    Ok(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": prepared.method,
        "context": {
            "id": session.session_id,
            "generation": session.generation,
        },
        "params": prepared.params,
    }))
}

fn validate_response(
    session: &OpenedProtocolSession,
    request: &Value,
    prepared: &PreparedRequest,
    response_bytes: Vec<u8>,
    protocol_contracts: &ProtocolContractCatalog,
) -> Result<Value, LocalFailure> {
    let response_byte_count = response_bytes.len();
    if response_byte_count > session.max_response_bytes {
        return Err(response_limit_failure(
            session.max_response_bytes,
            response_byte_count,
        ));
    }
    let response = parse_strict_json_bytes(&response_bytes).map_err(|_| peer_contract_failure())?;
    protocol_contracts
        .validate_response(prepared.method, &response)
        .map_err(|_| peer_contract_failure())?;
    if response.get("id") != request.get("id") {
        return Err(peer_contract_failure());
    }
    if response.get("error").is_some()
        && response
            .pointer("/error/data/kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("action."))
        && prepared.method != "semantic.invoke"
    {
        return Err(peer_contract_failure());
    }
    let Some(result) = response.get("result") else {
        return Ok(response);
    };
    match prepared.method {
        "semantic.list" => {
            validate_list_response(session, request, result, response_byte_count)?;
        }
        "semantic.show" => {
            if result.get("id") != prepared.params.get("capability")
                || result.get("declarationRevision") != prepared.params.get("declarationRevision")
            {
                return Err(peer_contract_failure());
            }
        }
        "semantic.schema" => {
            if result.get("schema") != prepared.params.get("schema") {
                return Err(peer_contract_failure());
            }
        }
        "semantic.query" => {
            if result.get("valueSchema") != prepared.params.get("valueSchema") {
                return Err(peer_contract_failure());
            }
            let value = result.get("value").ok_or_else(peer_contract_failure)?;
            let canonical_bytes = json_stringify_len(value) as u64;
            if result.get("bytes").and_then(Value::as_u64) != Some(canonical_bytes) {
                return Err(peer_contract_failure());
            }
        }
        "semantic.invoke" => {
            if result.get("capability") != prepared.params.get("capability")
                || result.get("declarationRevision") != prepared.params.get("declarationRevision")
            {
                return Err(peer_contract_failure());
            }
        }
        _ => return Err(peer_contract_failure()),
    }
    Ok(response)
}

fn json_stringify_len(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => js_number_string(
            number
                .as_f64()
                .expect("JSON numbers are finite and representable as f64"),
        )
        .len(),
        Value::String(_) => serde_json::to_vec(value)
            .expect("a JSON string serializes")
            .len(),
        Value::Array(values) => {
            2 + values.iter().map(json_stringify_len).sum::<usize>()
                + values.len().saturating_sub(1)
        }
        Value::Object(object) => {
            2 + object
                .iter()
                .map(|(key, value)| {
                    serde_json::to_vec(key)
                        .expect("an object key serializes")
                        .len()
                        + 1
                        + json_stringify_len(value)
                })
                .sum::<usize>()
                + object.len().saturating_sub(1)
        }
    }
}

fn js_number_string(value: f64) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    let negative = value.is_sign_negative();
    let absolute = value.abs();
    let fixed = absolute.to_string();
    let mut formatted = if absolute >= 1e21 {
        let digits = fixed.trim_end_matches('0');
        let coefficient = if digits.len() == 1 {
            digits.to_owned()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{coefficient}e+{}", fixed.len() - 1)
    } else if absolute < 1e-6 {
        let fractional = fixed
            .strip_prefix("0.")
            .expect("small finite numbers use fixed decimal formatting");
        let first = fractional
            .find(|character| character != '0')
            .expect("non-zero numbers have a significant digit");
        let digits = fractional[first..].trim_end_matches('0');
        let coefficient = if digits.len() == 1 {
            digits.to_owned()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{coefficient}e-{}", first + 1)
    } else {
        fixed
    };
    if negative {
        formatted.insert(0, '-');
    }
    formatted
}

fn validate_list_response(
    session: &OpenedProtocolSession,
    request: &Value,
    result: &Value,
    response_byte_count: usize,
) -> Result<(), LocalFailure> {
    if result
        .pointer("/catalog/generation")
        .and_then(Value::as_u64)
        != Some(session.generation)
    {
        return Err(peer_contract_failure());
    }
    let capabilities = result
        .get("capabilities")
        .and_then(Value::as_array)
        .ok_or_else(peer_contract_failure)?;
    let mut ids = std::collections::BTreeSet::new();
    if capabilities.iter().any(|item| {
        item.get("id")
            .and_then(Value::as_str)
            .is_none_or(|id| !ids.insert(id))
    }) {
        return Err(peer_contract_failure());
    }
    let page = result.get("page").ok_or_else(peer_contract_failure)?;
    let returned_items = page
        .get("returnedItems")
        .and_then(Value::as_u64)
        .ok_or_else(peer_contract_failure)? as usize;
    let applied_items = page
        .pointer("/appliedLimits/maxItems")
        .and_then(Value::as_u64)
        .ok_or_else(peer_contract_failure)? as usize;
    let applied_bytes = page
        .pointer("/appliedLimits/maxBytes")
        .and_then(Value::as_u64)
        .ok_or_else(peer_contract_failure)? as usize;
    let requested_items = request
        .pointer("/params/limits/maxItems")
        .and_then(Value::as_u64)
        .map_or(session.max_page_items, |value| value as usize);
    let requested_bytes = request
        .pointer("/params/limits/maxBytes")
        .and_then(Value::as_u64)
        .map_or(session.max_response_bytes, |value| value as usize);
    if returned_items != capabilities.len()
        || returned_items > applied_items
        || applied_items > requested_items
        || applied_items > session.max_page_items
        || applied_bytes > requested_bytes
        || applied_bytes > session.max_response_bytes
        || response_byte_count > applied_bytes
    {
        return Err(peer_contract_failure());
    }
    Ok(())
}

fn response_limit_failure(limit: usize, actual: usize) -> LocalFailure {
    LocalFailure {
        error: structured_error(
            "resourceExhausted",
            "A safe bounded result cannot be produced.",
            false,
            details_from([
                ("limit", Value::from(limit as u64)),
                ("actual_bytes", Value::from(actual as u64)),
            ]),
        ),
        next_actions: Vec::new(),
    }
}

fn peer_contract_failure() -> LocalFailure {
    LocalFailure {
        error: structured_error(
            "invalidRequest",
            "The catalog peer returned an invalid protocol response.",
            false,
            Map::new(),
        ),
        next_actions: Vec::new(),
    }
}

fn map_response(
    executable: &str,
    subcommand: &str,
    session: &OpenedProtocolSession,
    prepared: &PreparedRequest,
    response: Value,
) -> Result<CatalogOutput, LocalFailure> {
    if response.get("error").is_some() {
        return map_error(executable, session, prepared, &response);
    }
    let Some(result) = response.get("result") else {
        return Err(internal_error());
    };
    match subcommand {
        "list" => map_list(executable, session, result),
        "show" => map_show(result),
        "schema" => map_schema(result),
        "query" => map_query(prepared.capability.as_deref(), result),
        "invoke" => map_invoke(result),
        _ => unreachable!("the registry owns every catalog command"),
    }
}

fn map_list(
    executable: &str,
    session: &OpenedProtocolSession,
    result: &Value,
) -> Result<CatalogOutput, LocalFailure> {
    let catalog = result.get("catalog").ok_or_else(internal_error)?;
    let capabilities = result
        .get("capabilities")
        .and_then(Value::as_array)
        .ok_or_else(internal_error)?;
    let items = capabilities
        .iter()
        .map(project_catalog_item)
        .collect::<Result<Vec<_>, _>>()?;
    let data = serde_json::json!({
        "catalog": {
            "id": catalog.get("id").ok_or_else(internal_error)?,
            "generation": catalog.get("generation").ok_or_else(internal_error)?,
        },
        "capabilities": items,
    });
    let (disclosure, mut next_actions) = project_page(result.get("page"), items.len())?;
    if disclosure.truncated
        && let Some(cursor) = disclosure.next_cursor.clone()
    {
        next_actions.insert(
            0,
            NextAction {
                id: "catalog.list.continue",
                argv: vec![
                    executable.to_owned(),
                    "catalog".to_owned(),
                    "list".to_owned(),
                    argv_value("--session", &session.session_id),
                    argv_value("--target", &session.target_id),
                    "--cursor".to_owned(),
                    cursor,
                    "--output".to_owned(),
                    "json".to_owned(),
                ],
                side_effect: SideEffect::ReadOnly,
                retry_safety: RetrySafety::Safe,
                preconditions: vec!["session is still valid"],
                reason: "Continue the truncated Semantic Catalog list",
            },
        );
    }
    let count = items.len();
    let human_summary = if disclosure.truncated {
        format!("Semantic Catalog: {count} capabilities truncated.")
    } else {
        format!("Semantic Catalog: {count} capabilities.")
    };
    let mut context = OutcomeContext::new(disclosure);
    context.next_actions = next_actions;
    Ok(CatalogOutput {
        command: vec!["catalog".to_owned(), "list".to_owned()],
        outcome: HandlerOutcome::Succeeded { data, context },
        human_summary,
    })
}

fn map_show(result: &Value) -> Result<CatalogOutput, LocalFailure> {
    let kind = result
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(internal_error)?;
    let id = result
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(internal_error)?;
    let declaration_revision = result
        .get("declarationRevision")
        .cloned()
        .ok_or_else(internal_error)?;
    let data = match kind {
        "resource" => {
            let mut object = serde_json::json!({
                "id": id,
                "kind": "resource",
                "declaration_revision": declaration_revision,
                "value_schema": project_handle(result.get("valueSchema").ok_or_else(internal_error)?)?,
            });
            if let Some(input_schema) = result.get("inputSchema") {
                object["input_schema"] = project_handle(input_schema)?;
            }
            object
        }
        "action" => serde_json::json!({
            "id": id,
            "kind": "action",
            "declaration_revision": declaration_revision,
            "input_schema": project_handle(result.get("inputSchema").ok_or_else(internal_error)?)?,
            "policy": project_policy(result.get("policy").ok_or_else(internal_error)?)?,
        }),
        _ => return Err(internal_error()),
    };
    Ok(CatalogOutput {
        command: vec!["catalog".to_owned(), "show".to_owned()],
        outcome: HandlerOutcome::Succeeded {
            data,
            context: OutcomeContext::new(Disclosure::complete(1)),
        },
        human_summary: format!("Semantic Capability: {id} ({kind})."),
    })
}

fn map_schema(result: &Value) -> Result<CatalogOutput, LocalFailure> {
    let handle = project_handle(result.get("schema").ok_or_else(internal_error)?)?;
    let document = result.get("document").cloned().ok_or_else(internal_error)?;
    let schema_id = handle
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("schema")
        .to_owned();
    Ok(CatalogOutput {
        command: vec!["catalog".to_owned(), "schema".to_owned()],
        outcome: HandlerOutcome::Succeeded {
            data: serde_json::json!({
                "schema": handle,
                "document": document,
            }),
            context: OutcomeContext::new(Disclosure::complete(1)),
        },
        human_summary: format!("Live App schema: {schema_id}."),
    })
}

fn map_query(capability: Option<&str>, result: &Value) -> Result<CatalogOutput, LocalFailure> {
    let bytes = result
        .get("bytes")
        .and_then(Value::as_u64)
        .ok_or_else(internal_error)?;
    let id = capability.unwrap_or("resource");
    Ok(CatalogOutput {
        command: vec!["catalog".to_owned(), "query".to_owned()],
        outcome: HandlerOutcome::Succeeded {
            data: serde_json::json!({
                "value": result.get("value").cloned().ok_or_else(internal_error)?,
                "value_schema": project_handle(result.get("valueSchema").ok_or_else(internal_error)?)?,
                "bytes": bytes,
            }),
            context: OutcomeContext::new(Disclosure::complete(1)),
        },
        human_summary: format!("Queried Semantic Resource {id} ({bytes} bytes)."),
    })
}

fn map_invoke(result: &Value) -> Result<CatalogOutput, LocalFailure> {
    let capability = result
        .get("capability")
        .and_then(Value::as_str)
        .ok_or_else(internal_error)?;
    if result.get("completed") != Some(&Value::Bool(true)) {
        return Err(internal_error());
    }
    Ok(CatalogOutput {
        command: vec!["catalog".to_owned(), "invoke".to_owned()],
        outcome: HandlerOutcome::Succeeded {
            data: serde_json::json!({
                "capability": capability,
                "declaration_revision": result.get("declarationRevision").ok_or_else(internal_error)?,
                "completed": true,
            }),
            context: OutcomeContext::new(Disclosure::complete(1)),
        },
        human_summary: format!("Invoked Semantic Action {capability}."),
    })
}

fn map_error(
    executable: &str,
    session: &OpenedProtocolSession,
    prepared: &PreparedRequest,
    response: &Value,
) -> Result<CatalogOutput, LocalFailure> {
    let error = response.get("error").ok_or_else(internal_error)?;
    let kind = error
        .pointer("/data/kind")
        .and_then(Value::as_str)
        .unwrap_or("cli.internalError");
    let retryable = error
        .pointer("/data/retryable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let details = project_safe_details(error.pointer("/data/details"));
    if kind == "action.outcomeUnknown" {
        return Ok(outcome_unknown(
            executable,
            session,
            prepared.capability.as_deref(),
            prepared.declaration_revision,
        ));
    }
    let message = message_for(kind);
    let next_actions = inspect_actions(
        executable,
        session,
        prepared.capability.as_deref(),
        prepared.declaration_revision,
    );
    Err(LocalFailure {
        error: structured_error(
            static_kind(kind),
            message,
            retryable && kind != "action.outcomeUnknown",
            details,
        ),
        next_actions,
    })
}

fn outcome_unknown(
    executable: &str,
    session: &OpenedProtocolSession,
    capability: Option<&str>,
    declaration_revision: Option<u64>,
) -> CatalogOutput {
    let mut context = OutcomeContext::new(Disclosure::complete(0));
    context.next_actions = inspect_actions(executable, session, capability, declaration_revision);
    CatalogOutput {
        command: vec!["catalog".to_owned(), "invoke".to_owned()],
        outcome: HandlerOutcome::Failed {
            error: structured_error(
                "action.outcomeUnknown",
                "The mutation may have executed; do not replay it.",
                false,
                details_from(
                    capability
                        .map(|capability| ("capability", Value::String(capability.to_owned()))),
                ),
            ),
            context,
        },
        human_summary: "The mutation may have executed; do not replay it.".to_owned(),
    }
}

fn project_page(
    page: Option<&Value>,
    fallback_items: usize,
) -> Result<(Disclosure, Vec<NextAction>), LocalFailure> {
    let Some(page) = page else {
        return Ok((Disclosure::complete(fallback_items), Vec::new()));
    };
    let truncated = page
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or_else(internal_error)?;
    let returned_items = page
        .get("returnedItems")
        .and_then(Value::as_u64)
        .ok_or_else(internal_error)? as usize;
    let applied = page
        .get("appliedLimits")
        .ok_or_else(peer_contract_failure)?;
    let applied_limits = AppliedLimits {
        max_items: applied
            .get("maxItems")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        max_bytes: applied
            .get("maxBytes")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
    };
    if !truncated {
        return Ok((
            Disclosure::complete_with_limits(returned_items, applied_limits),
            Vec::new(),
        ));
    }
    let next_cursor = page
        .get("nextCursor")
        .and_then(Value::as_str)
        .ok_or_else(internal_error)?;
    let reasons = page
        .get("reasons")
        .and_then(Value::as_array)
        .ok_or_else(peer_contract_failure)?
        .iter()
        .map(|reason| match reason.as_str() {
            Some("maxItems") => Ok("max_items"),
            Some("maxBytes") => Ok("max_bytes"),
            Some("providerDeadline") => Ok("provider_deadline"),
            Some("providerLimit") => Ok("provider_limit"),
            _ => Err(peer_contract_failure()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        Disclosure::truncated(returned_items, applied_limits, reasons, next_cursor),
        Vec::new(),
    ))
}

fn project_catalog_item(item: &Value) -> Result<Value, LocalFailure> {
    Ok(serde_json::json!({
        "id": item.get("id").ok_or_else(internal_error)?,
        "kind": item.get("kind").ok_or_else(internal_error)?,
        "declaration_revision": item.get("declarationRevision").ok_or_else(internal_error)?,
    }))
}

fn project_handle(value: &Value) -> Result<Value, LocalFailure> {
    Ok(serde_json::json!({
        "id": value.get("id").ok_or_else(internal_error)?,
        "revision": value.get("revision").ok_or_else(internal_error)?,
        "digest": value.get("digest").ok_or_else(internal_error)?,
    }))
}

fn project_policy(value: &Value) -> Result<Value, LocalFailure> {
    let authorization = match value.get("authorization").and_then(Value::as_str) {
        Some("none") => "none",
        Some("destructiveAuthorization") => "destructive_authorization",
        _ => return Err(internal_error()),
    };
    let retry_safety = match value.get("retrySafety").and_then(Value::as_str) {
        Some("noAutomaticRetry") => "no_automatic_retry",
        Some("retryWithProofOnly") => "retry_with_proof_only",
        _ => return Err(internal_error()),
    };
    Ok(serde_json::json!({
        "authorization": authorization,
        "retry_safety": retry_safety,
    }))
}

fn project_safe_details(details: Option<&Value>) -> Map<String, Value> {
    let Some(Value::Object(object)) = details else {
        return Map::new();
    };
    let mut projected = Map::new();
    for (key, value) in object {
        match key.as_str() {
            "supportedMajor" => {
                projected.insert("supported_major".to_owned(), value.clone());
            }
            "method" | "limit" | "capability" | "field" => {
                projected.insert(key.clone(), value.clone());
            }
            "snapshotId" => {
                projected.insert("snapshot_id".to_owned(), value.clone());
            }
            "actualBytes" => {
                projected.insert("actual_bytes".to_owned(), value.clone());
            }
            "declarationRevision" => {
                projected.insert("declaration_revision".to_owned(), value.clone());
            }
            "schema" => {
                if let Ok(handle) = project_handle(value) {
                    projected.insert("schema".to_owned(), handle);
                }
            }
            _ => {}
        }
    }
    projected
}

fn inspect_actions(
    executable: &str,
    session: &OpenedProtocolSession,
    capability: Option<&str>,
    declaration_revision: Option<u64>,
) -> Vec<NextAction> {
    let mut actions = vec![NextAction {
        id: "catalog.list",
        argv: vec![
            executable.to_owned(),
            "catalog".to_owned(),
            "list".to_owned(),
            argv_value("--session", &session.session_id),
            argv_value("--target", &session.target_id),
            "--output".to_owned(),
            "json".to_owned(),
        ],
        side_effect: SideEffect::ReadOnly,
        retry_safety: RetrySafety::Safe,
        preconditions: vec!["session is still valid"],
        reason: "Inspect the current Semantic Catalog without replaying a mutation",
    }];
    if let (Some(capability), Some(revision)) = (capability, declaration_revision) {
        actions.push(NextAction {
            id: "catalog.show",
            argv: vec![
                executable.to_owned(),
                "catalog".to_owned(),
                "show".to_owned(),
                "--capability".to_owned(),
                capability.to_owned(),
                "--declaration-revision".to_owned(),
                revision.to_string(),
                argv_value("--session", &session.session_id),
                argv_value("--target", &session.target_id),
                "--output".to_owned(),
                "json".to_owned(),
            ],
            side_effect: SideEffect::ReadOnly,
            retry_safety: RetrySafety::Safe,
            preconditions: vec!["session is still valid"],
            reason: "Show the capability declaration without invoking it",
        });
    }
    actions
}

fn select_error(executable: &str, error: CatalogSelectError) -> LocalFailure {
    match error {
        CatalogSelectError::SessionSelectionRequired { mut candidates } => {
            candidates.sort_by(|left, right| {
                left.session_id
                    .cmp(&right.session_id)
                    .then(left.target_id.cmp(&right.target_id))
            });
            LocalFailure {
                error: structured_error(
                    "session.selectionRequired",
                    "Select one opened Protocol Session; live catalog access is not available offline.",
                    false,
                    Map::new(),
                ),
                next_actions: selection_actions(executable, &candidates),
            }
        }
        CatalogSelectError::TargetSelectionRequired { mut candidates } => {
            candidates.sort_by(|left, right| {
                left.session_id
                    .cmp(&right.session_id)
                    .then(left.target_id.cmp(&right.target_id))
            });
            LocalFailure {
                error: structured_error(
                    "target.selectionRequired",
                    "Select one Target; catalog commands never guess a current Target.",
                    false,
                    Map::new(),
                ),
                next_actions: selection_actions(executable, &candidates),
            }
        }
        CatalogSelectError::SessionExpired => LocalFailure {
            error: structured_error("sessionExpired", "Session expired.", false, Map::new()),
            next_actions: discovery_actions(executable),
        },
        CatalogSelectError::AuthenticationRequired => LocalFailure {
            error: structured_error(
                "transport.authenticationRequired",
                "Authentication is required.",
                false,
                Map::new(),
            ),
            next_actions: discovery_actions(executable),
        },
    }
}

fn selection_actions(executable: &str, candidates: &[SessionCandidate]) -> Vec<NextAction> {
    if candidates.is_empty() {
        return discovery_actions(executable);
    }
    candidates
        .iter()
        .filter(|candidate| {
            is_session_id(&candidate.session_id) && is_target_id(&candidate.target_id)
        })
        .take(32)
        .map(|candidate| NextAction {
            id: "catalog.list.select",
            argv: vec![
                executable.to_owned(),
                "catalog".to_owned(),
                "list".to_owned(),
                argv_value("--session", &candidate.session_id),
                argv_value("--target", &candidate.target_id),
                "--output".to_owned(),
                "json".to_owned(),
            ],
            side_effect: SideEffect::ReadOnly,
            retry_safety: RetrySafety::Safe,
            preconditions: Vec::new(),
            reason: "Retry with an explicit Target and Protocol Session",
        })
        .collect()
}

fn discovery_actions(executable: &str) -> Vec<NextAction> {
    vec![
        NextAction {
            id: "capabilities",
            argv: vec![
                executable.to_owned(),
                "capabilities".to_owned(),
                "--output".to_owned(),
                "json".to_owned(),
            ],
            side_effect: SideEffect::ReadOnly,
            retry_safety: RetrySafety::Safe,
            preconditions: Vec::new(),
            reason: "Discover the installed CLI contract",
        },
        NextAction {
            id: "doctor",
            argv: vec![
                executable.to_owned(),
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
    ]
}

fn invalid_invocation(field: &str) -> LocalFailure {
    LocalFailure {
        error: structured_error(
            "cli.invalidInvocation",
            "Invalid CLI invocation. Use built-in help or capability discovery.",
            false,
            details_from([("field", Value::String(field.to_owned()))]),
        ),
        next_actions: Vec::new(),
    }
}

fn internal_error() -> LocalFailure {
    LocalFailure {
        error: structured_error(
            "cli.internalError",
            "The CLI runtime failed unexpectedly.",
            false,
            Map::new(),
        ),
        next_actions: Vec::new(),
    }
}

fn structured_error(
    kind: &'static str,
    message: &'static str,
    retryable: bool,
    details: Map<String, Value>,
) -> StructuredError {
    StructuredError {
        kind,
        message,
        retryable,
        details,
    }
}

fn details_from<I>(pairs: I) -> Map<String, Value>
where
    I: IntoIterator<Item = (&'static str, Value)>,
{
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn static_kind(kind: &str) -> &'static str {
    match kind {
        "capabilityUnavailable" => "capabilityUnavailable",
        "incompatibleProtocol" => "incompatibleProtocol",
        "invalidParams" => "invalidParams",
        "invalidRequest" => "invalidRequest",
        "methodNotFound" => "methodNotFound",
        "parseError" => "parseError",
        "resourceExhausted" => "resourceExhausted",
        "sessionExpired" => "sessionExpired",
        "cursorExpired" => "cursorExpired",
        "timeout" => "timeout",
        "internalError" => "internalError",
        "semantic.capabilityNotFound" => "semantic.capabilityNotFound",
        "semantic.schemaMismatch" => "semantic.schemaMismatch",
        "semantic.unavailable" => "semantic.unavailable",
        "semantic.disclosureDenied" => "semantic.disclosureDenied",
        "action.policyDenied" => "action.policyDenied",
        "action.conflict" => "action.conflict",
        "action.outcomeUnknown" => "action.outcomeUnknown",
        "cli.internalError" => "cli.internalError",
        "cli.invalidInvocation" => "cli.invalidInvocation",
        _ => "cli.internalError",
    }
}

fn message_for(kind: &str) -> &'static str {
    match kind {
        "semantic.capabilityNotFound" | "semantic.unavailable" => {
            "Semantic capability is unavailable."
        }
        "semantic.schemaMismatch" => "Semantic schema does not match.",
        "semantic.disclosureDenied" => "Semantic disclosure is denied.",
        "action.policyDenied" => "Action policy is denied.",
        "action.conflict" => "Action conflicts with an in-flight mutation.",
        "action.outcomeUnknown" => "The mutation may have executed; do not replay it.",
        "capabilityUnavailable" => "Capability unavailable.",
        "sessionExpired" => "Session expired.",
        "cursorExpired" => "The continuation cursor is no longer valid.",
        "invalidParams" => "Method parameters are invalid.",
        "incompatibleProtocol" => "No compatible protocol version exists.",
        "timeout" => "The operation exceeded its deadline.",
        "resourceExhausted" => "A safe bounded result cannot be produced.",
        _ => "The catalog operation failed.",
    }
}

fn required_flag<'a>(matches: &'a ArgMatches, id: &str) -> Result<&'a str, LocalFailure> {
    matches
        .get_one::<String>(id)
        .map(String::as_str)
        .ok_or_else(|| invalid_invocation(id))
}

fn optional_flag<'a>(matches: &'a ArgMatches, id: &str) -> Option<&'a str> {
    matches.get_one::<String>(id).map(String::as_str)
}

fn parse_capability(value: &str) -> Result<String, LocalFailure> {
    if is_capability_id(value) {
        Ok(value.to_owned())
    } else {
        Err(invalid_invocation("capability"))
    }
}

fn parse_revision(value: &str) -> Result<u64, LocalFailure> {
    parse_bound(value, 1, u64::MAX, "declarationRevision")
}

fn parse_bound(value: &str, min: u64, max: u64, field: &'static str) -> Result<u64, LocalFailure> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| invalid_invocation(field))?;
    if parsed < min || parsed > max {
        Err(invalid_invocation(field))
    } else {
        Ok(parsed)
    }
}

fn parse_handle(id: &str, revision: &str, digest: &str) -> Result<Value, LocalFailure> {
    if !is_schema_id(id) {
        return Err(invalid_invocation("schema"));
    }
    if !is_digest(digest) {
        return Err(invalid_invocation("schema"));
    }
    Ok(serde_json::json!({
        "id": id,
        "revision": parse_bound(revision, 1, u64::MAX, "schema")?,
        "digest": digest,
    }))
}

fn parse_input_json(value: &str) -> Result<Value, LocalFailure> {
    if value.len() > INPUT_JSON_MAX_BYTES {
        let mut failure = invalid_invocation("input");
        failure.error.details = details_from([
            ("field", Value::String("input".to_owned())),
            ("limit_bytes", Value::from(INPUT_JSON_MAX_BYTES as u64)),
        ]);
        return Err(failure);
    }
    parse_strict_json(value).map_err(|_| invalid_invocation("input"))
}

fn is_capability_id(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    let mut index = 1;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            index += 1;
            continue;
        }
        if matches!(byte, b'.' | b'_' | b'-') {
            index += 1;
            if index >= bytes.len()
                || !(bytes[index].is_ascii_lowercase() || bytes[index].is_ascii_digit())
            {
                return false;
            }
            continue;
        }
        return false;
    }
    true
}

fn is_schema_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("schema_") else {
        return false;
    };
    (8..=120).contains(&rest.len())
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-'))
}

fn is_digest(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("sha256:") else {
        return false;
    };
    rest.len() == 64
        && rest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_session_id(value: &str) -> bool {
    (16..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-'))
}

fn is_target_id(value: &str) -> bool {
    (1..=4087).contains(&value.chars().count())
}

fn is_protocol_capability(value: &str) -> bool {
    if !(3..=128).contains(&value.len()) || !value.is_ascii() {
        return false;
    }
    let mut segments = value.split('.');
    let valid_segment = |segment: &str| {
        !segment.is_empty()
            && segment.as_bytes()[0].is_ascii_lowercase()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    };
    let Some(first) = segments.next() else {
        return false;
    };
    valid_segment(first) && segments.clone().next().is_some() && segments.all(valid_segment)
}

fn argv_value(flag: &str, value: &str) -> String {
    format!("{flag}={value}")
}

fn parse_strict_json(input: &str) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = StrictValueSeed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

fn parse_strict_json_bytes(input: &[u8]) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let value = StrictValueSeed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

struct StrictValueSeed;

impl<'de> DeserializeSeed<'de> for StrictValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("JSON numbers must be finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        StrictValueSeed.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or_default());
        while let Some(value) = sequence.next_element_seed(StrictValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom(format!(
                    "duplicate JSON object key: {key}"
                )));
            }
            values.insert(key, object.next_value_seed(StrictValueSeed)?);
        }
        Ok(Value::Object(values))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CatalogOutput, FakeCatalogRuntime, INPUT_JSON_MAX_BYTES, OpenedProtocolSession,
        js_number_string, run,
    };
    use crate::protocol::ProtocolContractCatalog;
    use crate::registry::command_model;
    use crate::result::HandlerOutcome;
    use std::sync::atomic::AtomicU64;

    fn opened_session(capabilities: &[&str], minor: u32) -> OpenedProtocolSession {
        OpenedProtocolSession {
            session_id: "session_0123456789abcdef".to_owned(),
            generation: 7,
            target_id: "target_demo".to_owned(),
            protocol_major: 1,
            protocol_minor: minor,
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            max_request_bytes: 4096,
            max_response_bytes: 4096,
            max_page_items: 2,
        }
    }

    fn catalog_run(runtime: &FakeCatalogRuntime, args: &[&str]) -> CatalogOutput {
        let matches = command_model("fixture-cli", "0.1.0")
            .try_get_matches_from(args)
            .expect("argv parses");
        let (name, catalog) = matches.subcommand().expect("catalog command");
        assert_eq!(name, "catalog");
        let (subcommand, submatches) = catalog.subcommand().expect("catalog subcommand");
        run(
            "fixture-cli",
            subcommand,
            submatches,
            runtime,
            &ProtocolContractCatalog::new().expect("protocol contracts initialize"),
            &AtomicU64::new(1),
        )
    }

    fn failed(output: &CatalogOutput) -> &crate::result::StructuredError {
        match &output.outcome {
            HandlerOutcome::Failed { error, .. } => error,
            other => panic!("expected failure, got {other:?}"),
        }
    }

    fn digest() -> &'static str {
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }

    #[test]
    fn query_byte_count_uses_ecmascript_number_formatting() {
        for (number, expected) in [
            (1.0, "1"),
            (-0.0, "0"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
        ] {
            assert_eq!(js_number_string(number), expected);
        }
    }

    #[test]
    fn oversized_and_malformed_input_fail_before_exchange() {
        let runtime = FakeCatalogRuntime::new();
        runtime.add_session(opened_session(&["session.core", "semantic.catalog"], 2));
        let oversized = format!("{{\"k\":\"{}\"}}", "x".repeat(INPUT_JSON_MAX_BYTES));
        for input in [oversized.as_str(), "{", "{\"a\":1,\"a\":2}"] {
            let output = catalog_run(
                &runtime,
                &[
                    "fixture-cli",
                    "catalog",
                    "invoke",
                    "--capability",
                    "account.delete",
                    "--declaration-revision",
                    "3",
                    "--input-schema-id",
                    "schema_action0001",
                    "--input-schema-revision",
                    "1",
                    "--input-schema-digest",
                    digest(),
                    "--input",
                    input,
                    "--session",
                    "session_0123456789abcdef",
                ],
            );
            assert_eq!(failed(&output).kind, "cli.invalidInvocation");
            assert!(!format!("{:?}", output.outcome).contains("grant_"));
        }
        assert!(runtime.exchange_requests().is_empty());
    }

    #[test]
    fn query_input_without_schema_does_not_exchange() {
        let runtime = FakeCatalogRuntime::new();
        runtime.add_session(opened_session(&["session.core", "semantic.catalog"], 2));
        let output = catalog_run(
            &runtime,
            &[
                "fixture-cli",
                "catalog",
                "query",
                "--capability",
                "config.current",
                "--declaration-revision",
                "1",
                "--value-schema-id",
                "schema_value0001",
                "--value-schema-revision",
                "1",
                "--value-schema-digest",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "--input",
                "{\"scope\":\"active\"}",
                "--session",
                "session_0123456789abcdef",
            ],
        );
        assert_eq!(failed(&output).kind, "cli.invalidInvocation");
        assert!(runtime.exchange_requests().is_empty());
    }

    #[test]
    fn unnegotiated_catalog_does_not_exchange() {
        let runtime = FakeCatalogRuntime::new();
        runtime.add_session(opened_session(&["session.core"], 2));
        let output = catalog_run(
            &runtime,
            &[
                "fixture-cli",
                "catalog",
                "list",
                "--session",
                "session_0123456789abcdef",
            ],
        );
        assert_eq!(failed(&output).kind, "capabilityUnavailable");
        assert!(runtime.exchange_requests().is_empty());
    }

    #[test]
    fn ambiguous_invoke_does_not_exchange_or_echo_grant() {
        let runtime = FakeCatalogRuntime::new();
        runtime.add_session(opened_session(&["session.core", "semantic.catalog"], 2));
        let mut other = opened_session(&["session.core", "semantic.catalog"], 2);
        other.session_id = "session_abcdef0123456789".to_owned();
        other.target_id = "target_other".to_owned();
        runtime.add_session(other);
        let output = catalog_run(
            &runtime,
            &[
                "fixture-cli",
                "catalog",
                "invoke",
                "--capability",
                "account.delete",
                "--declaration-revision",
                "3",
                "--input-schema-id",
                "schema_action0001",
                "--input-schema-revision",
                "1",
                "--input-schema-digest",
                digest(),
                "--input",
                "{\"account\":\"opaque-target\"}",
                "--authorization-grant",
                "grant_0123456789abcdef",
            ],
        );
        let error = failed(&output);
        assert_eq!(error.kind, "session.selectionRequired");
        assert!(!format!("{error:?}").contains("grant_"));
        assert!(!format!("{error:?}").contains("opaque-target"));
        match &output.outcome {
            HandlerOutcome::Failed { context, .. } => {
                for action in &context.next_actions {
                    assert!(!action.argv.iter().any(|token| token == "invoke"));
                    assert!(!action.argv.iter().any(|token| token.contains("grant_")));
                    assert!(!action.argv.iter().any(|token| token.contains("opaque")));
                }
            }
            _ => unreachable!(),
        }
        assert!(runtime.exchange_requests().is_empty());
    }

    #[test]
    fn invoke_encodes_protocol_request_and_redacts_grant_from_errors() {
        let runtime = FakeCatalogRuntime::new();
        runtime.add_session(opened_session(&["session.core", "semantic.catalog"], 2));
        runtime.set_responder(|_session, request| {
            assert_eq!(request["method"], "semantic.invoke");
            assert_eq!(
                request["params"]["authorizationGrant"],
                "grant_0123456789abcdef"
            );
            Ok(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": {
                    "code": -32026,
                    "message": "Action outcome is unknown",
                    "data": {
                        "kind": "action.outcomeUnknown",
                        "retryable": false,
                        "details": {"capability": "account.delete", "secret": "leak"}
                    }
                }
            }))
        });
        let output = catalog_run(
            &runtime,
            &[
                "fixture-cli",
                "catalog",
                "invoke",
                "--capability",
                "account.delete",
                "--declaration-revision",
                "3",
                "--input-schema-id",
                "schema_action0001",
                "--input-schema-revision",
                "1",
                "--input-schema-digest",
                digest(),
                "--input",
                "{\"account\":\"opaque-target\"}",
                "--authorization-grant",
                "grant_0123456789abcdef",
                "--session",
                "session_0123456789abcdef",
            ],
        );
        let error = failed(&output);
        assert_eq!(error.kind, "action.outcomeUnknown");
        assert!(!error.retryable);
        assert_eq!(error.details.get("secret"), None);
        match &output.outcome {
            HandlerOutcome::Failed { context, .. } => {
                assert!(
                    context
                        .next_actions
                        .iter()
                        .all(|action| !action.argv.iter().any(|token| token == "invoke"))
                );
                assert!(context.next_actions.iter().any(|action| {
                    action
                        .argv
                        .windows(2)
                        .any(|window| window == ["catalog", "list"])
                }));
            }
            _ => unreachable!(),
        }
        let rendered = format!("{:?}", output.outcome);
        assert!(!rendered.contains("grant_"));
        assert_eq!(runtime.exchange_requests().len(), 1);
    }

    #[test]
    fn list_projects_snake_case_catalog_page() {
        let runtime = FakeCatalogRuntime::new();
        runtime.add_session(opened_session(&["session.core", "semantic.catalog"], 2));
        runtime.set_responder(|_session, request| {
            assert_eq!(request["method"], "semantic.list");
            Ok(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "catalog": {"id": "catalog_12345678", "generation": 7},
                    "capabilities": [
                        {"id": "config.current", "kind": "resource", "declarationRevision": 1}
                    ],
                    "page": {
                        "truncated": true,
                        "returnedItems": 1,
                        "appliedLimits": {"maxItems": 1, "maxBytes": 4096},
                        "reasons": ["maxItems"],
                        "nextCursor": "cursor_catalog_page_2"
                    }
                }
            }))
        });
        let output = catalog_run(
            &runtime,
            &[
                "fixture-cli",
                "catalog",
                "list",
                "--session",
                "session_0123456789abcdef",
                "--max-items",
                "1",
            ],
        );
        match output.outcome {
            HandlerOutcome::Succeeded { data, context } => {
                assert_eq!(data["capabilities"][0]["declaration_revision"], 1);
                assert!(context.disclosure.truncated);
                assert!(
                    context.next_actions[0]
                        .argv
                        .windows(2)
                        .any(|window| window == ["--cursor", "cursor_catalog_page_2"])
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cursor_and_max_items_fail_before_exchange() {
        let runtime = FakeCatalogRuntime::new();
        runtime.add_session(opened_session(&["session.core", "semantic.catalog"], 2));
        let output = catalog_run(
            &runtime,
            &[
                "fixture-cli",
                "catalog",
                "list",
                "--cursor",
                "cursor_catalog_page_2",
                "--max-items",
                "1",
            ],
        );
        assert_eq!(failed(&output).kind, "cli.invalidInvocation");
        assert!(runtime.exchange_requests().is_empty());
    }
}
