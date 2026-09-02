//! Target-owned lifecycle supervisor above the sans-I/O Noise primitives.
//!
//! This module is deliberately transport-only: it owns no sockets, callbacks,
//! JSON parsing, protocol runtime, or Broker implementation.

use super::{
    BootstrapBinding, CloseReason, HandoffState, OuterFrameDecoder, ProcessBootstrapSecret,
    SessionBinding, TargetBootstrap, TargetLeaseConnection, TargetSession,
};
use minicbor::{Decoder, Encoder};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use std::collections::HashMap;
use zeroize::Zeroize;

pub const BOOTSTRAP_DEADLINE_MS: u64 = 10_000;
pub const SESSION_HANDSHAKE_DEADLINE_MS: u64 = 1_000;
pub const SESSION_OPEN_RESPONSE_DEADLINE_MS: u64 = 2_000;
pub const FRAME_DEADLINE_MS: u64 = 2_000;
pub const SESSION_IDLE_DEADLINE_MS: u64 = 30_000;
pub const LEASE_TICK_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventTag {
    BootstrapConnected,
    StreamBytes,
    FullWriteCommitted,
    SessionAccepted,
    RuntimeResponse,
    StreamEof,
    StreamIoFailed,
    StreamCloseNormal,
    TimerFired,
    EligibilityLost,
    CleanupFailed,
    InternalError,
}

pub struct Event<'a> {
    pub tag: EventTag,
    pub flags: u32,
    pub stream_id: u64,
    pub write_token: u64,
    pub bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutcomeKind {
    EndpointReady,
    WriteFrames,
    Application,
    LeaseReady,
    NeedInput,
    SessionTerminal,
    LeaseTerminal,
    Closed,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Outcome {
    pub kind: OutcomeKind,
    pub flags: u32,
    pub stream_id: u64,
    pub write_token: u64,
    pub bytes: Option<Vec<u8>>,
    pub value0: u64,
    pub value1: u64,
    pub next_deadline_ms: u64,
    pub close_reason: CloseReason,
    pub handoff: HandoffState,
    pub peer_close: Option<(CloseReason, HandoffState)>,
}

impl Drop for Outcome {
    fn drop(&mut self) {
        if let Some(bytes) = &mut self.bytes {
            bytes.zeroize();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorError {
    InvalidArgument,
    WrongPhase,
    Internal,
}

const JSON_SAFE_MAX: u64 = 9_007_199_254_740_991;
const DESCRIPTOR_MAX: usize = 8_192;

enum Endpoint {
    Ios(u16),
    Android(String),
}

struct ParsedDescriptor {
    binding: BootstrapBinding,
    broker_public_key: [u8; 32],
    endpoint: Endpoint,
}

enum Phase {
    AwaitBootstrap {
        binding: BootstrapBinding,
        broker_public_key: [u8; 32],
    },
    BootstrapM1Pending {
        stream_id: u64,
        bootstrap: TargetBootstrap,
        write_token: u64,
        decoder: OuterFrameDecoder,
        frame_timer: Option<u64>,
    },
    BootstrapM2 {
        stream_id: u64,
        bootstrap: TargetBootstrap,
        decoder: OuterFrameDecoder,
        frame_timer: Option<u64>,
    },
    BootstrapAckPending {
        stream_id: u64,
        lease: TargetLeaseConnection,
        pbs: ProcessBootstrapSecret,
        session_binding: SessionBinding,
        write_token: u64,
        decoder: OuterFrameDecoder,
        frame_timer: Option<u64>,
    },
    Eligible(Eligible),
    Terminal(Terminal),
}

struct Eligible {
    lease_stream: u64,
    lease: TargetLeaseConnection,
    pbs: ProcessBootstrapSecret,
    session_binding: SessionBinding,
    lease_decoder: OuterFrameDecoder,
    lease_write: Option<(u64, u64)>,
    lease_frame_timer: Option<u64>,
    lease_tick_timer: Option<u64>,
    lease_ticks: u8,
    missed_ticks: u8,
    children: HashMap<u64, Child>,
}

struct Child {
    phase: ChildPhase,
    decoder: OuterFrameDecoder,
    phase_timer: Option<u64>,
    frame_timer: Option<u64>,
    applications: u64,
}

enum ChildPhase {
    M1Pending {
        session: TargetSession,
        write_token: u64,
    },
    AwaitM2(TargetSession),
    FinishedPending {
        session: TargetSession,
        write_token: u64,
    },
    AwaitFinished(TargetSession),
    AwaitApplication(TargetSession),
    AwaitRuntime(TargetSession),
    ResponsePending {
        session: TargetSession,
        write_token: u64,
    },
    TerminalPending {
        write_token: u64,
        terminal: Terminal,
    },
    Terminal,
}

#[derive(Clone, Copy)]
struct Terminal {
    reason: CloseReason,
    handoff: HandoffState,
    peer_close: Option<(CloseReason, HandoffState)>,
}

#[derive(Clone, Copy)]
enum TimerAction {
    Bootstrap,
    BootstrapFrame,
    LeaseTick,
    LeaseFrame,
    SessionPhase(u64),
    SessionIdle(u64),
    SessionFrame(u64),
}

pub struct TargetTransport {
    phase: Phase,
    generation: u64,
    next_token: u64,
    timers: HashMap<u64, TimerAction>,
}

impl TargetTransport {
    pub fn create(descriptor_cbor: &[u8]) -> Result<(Self, Outcome), SupervisorError> {
        let parsed = parse_descriptor(descriptor_cbor)?;
        let generation = generate_process_generation()?;
        let (platform, endpoint_value, endpoint_bytes) = match parsed.endpoint {
            Endpoint::Ios(port) => (0, u64::from(port), None),
            Endpoint::Android(name) => (1, 0, Some(name.into_bytes())),
        };
        let transport = Self {
            phase: Phase::AwaitBootstrap {
                binding: parsed.binding,
                broker_public_key: parsed.broker_public_key,
            },
            generation,
            next_token: 1,
            timers: HashMap::new(),
        };
        // The Host owns platform launch and endpoint forwarding. A Target
        // cannot begin its bootstrap budget until the Host has made a real
        // connection and the listener has accepted it; otherwise Android
        // install/activity/forward latency consumes a handshake budget.
        let mut ready = Outcome::new(OutcomeKind::EndpointReady).values(platform, endpoint_value);
        ready.bytes = endpoint_bytes;
        Ok((transport, ready))
    }

    pub fn drive(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        if let Phase::Terminal(terminal) = &self.phase {
            let terminal = *terminal;
            return Ok(Outcome::terminal(
                OutcomeKind::LeaseTerminal,
                0,
                terminal.reason,
                terminal.handoff,
                terminal.peer_close,
            ));
        }
        if event.flags != 0 {
            #[cfg(test)]
            if event.flags == u32::MAX {
                panic!("test-only FFI panic injection");
            }
            return Err(SupervisorError::InvalidArgument);
        }
        match event.tag {
            EventTag::EligibilityLost => {
                if event.stream_id != 0 || event.write_token != 0 || !event.bytes.is_empty() {
                    return Err(SupervisorError::InvalidArgument);
                }
                Ok(self.eligibility_lost())
            }
            EventTag::CleanupFailed => {
                if event.stream_id != 0 || event.write_token != 0 || !event.bytes.is_empty() {
                    return Err(SupervisorError::InvalidArgument);
                }
                Ok(self.lease_terminal(CloseReason::CleanupFailed, None))
            }
            EventTag::InternalError => {
                if event.stream_id != 0 || event.write_token != 0 || !event.bytes.is_empty() {
                    return Err(SupervisorError::InvalidArgument);
                }
                Ok(self.lease_terminal(CloseReason::InternalError, None))
            }
            EventTag::TimerFired => self.timer_fired(event),
            EventTag::BootstrapConnected => self.bootstrap_connected(event),
            EventTag::FullWriteCommitted => self.full_write_committed(event),
            EventTag::SessionAccepted => self.session_accepted(event),
            EventTag::RuntimeResponse => self.runtime_response(event),
            EventTag::StreamBytes => self.stream_bytes(event),
            EventTag::StreamEof | EventTag::StreamIoFailed | EventTag::StreamCloseNormal => {
                self.stream_closed(event)
            }
        }
    }

    pub fn close(&mut self) -> Outcome {
        self.timers.clear();
        self.phase = Phase::Terminal(Terminal {
            reason: CloseReason::Normal,
            handoff: HandoffState::NotHandedOff,
            peer_close: None,
        });
        Outcome {
            kind: OutcomeKind::Closed,
            flags: 0,
            stream_id: 0,
            write_token: 0,
            bytes: None,
            value0: 0,
            value1: 0,
            next_deadline_ms: 0,
            close_reason: CloseReason::Normal,
            handoff: HandoffState::NotHandedOff,
            peer_close: None,
        }
    }

    pub fn terminate_internal_error(&mut self) {
        let _ = self.lease_terminal(CloseReason::InternalError, None);
    }

    pub fn terminal_reason(&self) -> Option<CloseReason> {
        match &self.phase {
            Phase::Terminal(terminal) => Some(terminal.reason),
            _ => None,
        }
    }

    pub fn terminal_timer_tokens(&self) -> impl Iterator<Item = u64> + '_ {
        self.timers.iter().filter_map(|(token, action)| {
            let terminal = match action {
                TimerAction::LeaseTick => matches!(
                    &self.phase,
                    Phase::Eligible(eligible)
                        if eligible.lease_tick_timer == Some(*token)
                            && (eligible.lease_ticks.saturating_add(1) >= 30
                                || eligible.missed_ticks.saturating_add(1) >= 4)
                ),
                TimerAction::Bootstrap
                | TimerAction::BootstrapFrame
                | TimerAction::LeaseFrame
                | TimerAction::SessionPhase(_)
                | TimerAction::SessionIdle(_)
                | TimerAction::SessionFrame(_) => true,
            };
            terminal.then_some(*token)
        })
    }

    fn bootstrap_connected(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        if event.stream_id == 0 || event.write_token != 0 || !event.bytes.is_empty() {
            return Err(SupervisorError::InvalidArgument);
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Terminal(Terminal::internal()));
        let Phase::AwaitBootstrap {
            binding,
            broker_public_key,
        } = phase
        else {
            self.phase = phase;
            return Err(SupervisorError::WrongPhase);
        };
        let mut bootstrap = match TargetBootstrap::new(binding, broker_public_key) {
            Ok(value) => value,
            Err(error) => return Ok(self.core_lease_error(error)),
        };
        let frames = match bootstrap.write_m1() {
            Ok(value) => value,
            Err(error) => return Ok(self.core_lease_error(error)),
        };
        let write_token = allocate_token(&mut self.next_token)?;
        let timer = schedule_timer(
            &mut self.next_token,
            &mut self.timers,
            TimerAction::Bootstrap,
        )?;
        self.phase = Phase::BootstrapM1Pending {
            stream_id: event.stream_id,
            bootstrap,
            write_token,
            decoder: OuterFrameDecoder::new(),
            frame_timer: None,
        };
        Ok(Outcome::write(event.stream_id, write_token, frames)
            .deadline(timer, BOOTSTRAP_DEADLINE_MS))
    }

    fn full_write_committed(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        if event.stream_id == 0 || event.write_token == 0 || !event.bytes.is_empty() {
            return Err(SupervisorError::InvalidArgument);
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Terminal(Terminal::internal()));
        match phase {
            Phase::BootstrapM1Pending {
                stream_id,
                bootstrap,
                write_token,
                decoder,
                frame_timer,
            } if stream_id == event.stream_id && write_token == event.write_token => {
                self.phase = Phase::BootstrapM2 {
                    stream_id,
                    bootstrap,
                    decoder,
                    frame_timer,
                };
                Ok(Outcome::new(OutcomeKind::NeedInput).stream(stream_id))
            }
            Phase::BootstrapAckPending {
                stream_id,
                lease,
                pbs,
                session_binding,
                write_token,
                decoder,
                frame_timer,
            } if stream_id == event.stream_id && write_token == event.write_token => {
                self.cancel_action(|action| matches!(action, TimerAction::Bootstrap));
                let tick = schedule_timer(
                    &mut self.next_token,
                    &mut self.timers,
                    TimerAction::LeaseTick,
                )?;
                let generation = session_binding.process_generation;
                let epoch = session_binding.listener_epoch;
                self.phase = Phase::Eligible(Eligible {
                    lease_stream: stream_id,
                    lease,
                    pbs,
                    session_binding,
                    lease_decoder: decoder,
                    lease_write: None,
                    lease_frame_timer: frame_timer,
                    lease_tick_timer: Some(tick),
                    lease_ticks: 0,
                    missed_ticks: 0,
                    children: HashMap::new(),
                });
                Ok(Outcome::new(OutcomeKind::LeaseReady)
                    .stream(stream_id)
                    .values(generation, epoch)
                    .deadline(tick, LEASE_TICK_MS))
            }
            Phase::Eligible(mut eligible) => {
                let result = if eligible.lease_stream == event.stream_id {
                    match eligible.lease_write {
                        Some((token, _)) if token == event.write_token => {
                            eligible.lease_write = None;
                            Ok(Outcome::new(OutcomeKind::NeedInput).stream(event.stream_id))
                        }
                        _ => Err(SupervisorError::WrongPhase),
                    }
                } else {
                    self.commit_child_write(&mut eligible, event.stream_id, event.write_token)
                };
                self.phase = Phase::Eligible(eligible);
                result
            }
            other => {
                self.phase = other;
                Err(SupervisorError::WrongPhase)
            }
        }
    }

    fn commit_child_write(
        &mut self,
        eligible: &mut Eligible,
        stream_id: u64,
        token: u64,
    ) -> Result<Outcome, SupervisorError> {
        let child = eligible
            .children
            .get_mut(&stream_id)
            .ok_or(SupervisorError::WrongPhase)?;
        let phase = std::mem::replace(&mut child.phase, ChildPhase::Terminal);
        match phase {
            ChildPhase::M1Pending {
                session,
                write_token,
            } if write_token == token => {
                child.phase = ChildPhase::AwaitM2(session);
                Ok(Outcome::new(OutcomeKind::NeedInput).stream(stream_id))
            }
            ChildPhase::FinishedPending {
                session,
                write_token,
            } if write_token == token => {
                child.phase = ChildPhase::AwaitFinished(session);
                Ok(Outcome::new(OutcomeKind::NeedInput).stream(stream_id))
            }
            ChildPhase::ResponsePending {
                session,
                write_token,
            } if write_token == token => {
                child.phase = ChildPhase::AwaitApplication(session);
                reset_child_phase_timer(
                    eligible,
                    &mut self.next_token,
                    &mut self.timers,
                    stream_id,
                    SESSION_IDLE_DEADLINE_MS,
                )
            }
            ChildPhase::TerminalPending {
                write_token,
                terminal,
            } if write_token == token => {
                child.phase = ChildPhase::Terminal;
                Ok(self.child_terminal(stream_id, terminal.reason, terminal.peer_close))
            }
            other => {
                child.phase = other;
                Err(SupervisorError::WrongPhase)
            }
        }
    }

    fn session_accepted(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        if event.stream_id == 0 || event.write_token != 0 || !event.bytes.is_empty() {
            return Err(SupervisorError::InvalidArgument);
        }
        let Phase::Eligible(eligible) = &mut self.phase else {
            return Err(SupervisorError::WrongPhase);
        };
        if event.stream_id == eligible.lease_stream
            || eligible.children.contains_key(&event.stream_id)
        {
            return Err(SupervisorError::WrongPhase);
        }
        let mut session = TargetSession::new(eligible.session_binding.clone(), &eligible.pbs)
            .map_err(|_| SupervisorError::Internal)?;
        let frames = session.write_m1().map_err(|_| SupervisorError::Internal)?;
        let token = allocate_token(&mut self.next_token)?;
        let timer = schedule_timer(
            &mut self.next_token,
            &mut self.timers,
            TimerAction::SessionPhase(event.stream_id),
        )?;
        eligible.children.insert(
            event.stream_id,
            Child {
                phase: ChildPhase::M1Pending {
                    session,
                    write_token: token,
                },
                decoder: OuterFrameDecoder::new(),
                phase_timer: Some(timer),
                frame_timer: None,
                applications: 0,
            },
        );
        Ok(Outcome::write(event.stream_id, token, frames)
            .with_deadline(timer, SESSION_HANDSHAKE_DEADLINE_MS))
    }

    fn runtime_response(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        if event.stream_id == 0 || event.write_token != 0 || event.bytes.is_empty() {
            return Err(SupervisorError::InvalidArgument);
        }
        let Phase::Eligible(eligible) = &mut self.phase else {
            return Err(SupervisorError::WrongPhase);
        };
        let child = eligible
            .children
            .get_mut(&event.stream_id)
            .ok_or(SupervisorError::WrongPhase)?;
        let phase = std::mem::replace(&mut child.phase, ChildPhase::Terminal);
        let ChildPhase::AwaitRuntime(mut session) = phase else {
            child.phase = phase;
            return Err(SupervisorError::WrongPhase);
        };
        let frame_parts = match session.write_application_response(event.bytes) {
            Ok(value) => value,
            Err(error) => return Ok(self.child_core_error(event.stream_id, error)),
        };
        let token = allocate_token(&mut self.next_token)?;
        child.phase = ChildPhase::ResponsePending {
            session,
            write_token: token,
        };
        Ok(Outcome::write(
            event.stream_id,
            token,
            concat_frames(frame_parts),
        ))
    }

    fn stream_bytes(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        if event.stream_id == 0 || event.write_token != 0 || event.bytes.is_empty() {
            return Err(SupervisorError::InvalidArgument);
        }
        match &self.phase {
            Phase::BootstrapM2 { stream_id, .. } if *stream_id == event.stream_id => {
                self.bootstrap_bytes(event)
            }
            Phase::Eligible(eligible) if eligible.lease_stream == event.stream_id => {
                self.lease_bytes(event)
            }
            Phase::Eligible(eligible) if eligible.children.contains_key(&event.stream_id) => {
                self.session_bytes(event)
            }
            _ => Err(SupervisorError::WrongPhase),
        }
    }

    fn bootstrap_bytes(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        let phase = std::mem::replace(&mut self.phase, Phase::Terminal(Terminal::internal()));
        let Phase::BootstrapM2 {
            stream_id,
            bootstrap,
            mut decoder,
            mut frame_timer,
        } = phase
        else {
            self.phase = phase;
            return Err(SupervisorError::WrongPhase);
        };
        let frames = match decoder.push(event.bytes) {
            Ok(value) => value,
            Err(error) => return Ok(self.core_lease_error(error)),
        };
        let frame_deadline = update_frame_timer(
            decoder.is_incomplete(),
            &mut frame_timer,
            &mut self.next_token,
            &mut self.timers,
            TimerAction::BootstrapFrame,
        )?;
        if frames.is_empty() {
            self.phase = Phase::BootstrapM2 {
                stream_id,
                bootstrap,
                decoder,
                frame_timer,
            };
            let outcome = Outcome::new(OutcomeKind::NeedInput).stream(stream_id);
            return Ok(match frame_deadline {
                Some(timer) => outcome.deadline(timer, FRAME_DEADLINE_MS),
                None => outcome,
            });
        }
        if frames.len() != 1 {
            return Ok(self.lease_terminal(CloseReason::SequenceViolation, None));
        }
        let (sender, pbs) = match bootstrap.read_m2(&frames[0], self.generation, 1) {
            Ok(value) => value,
            Err(error) => return Ok(self.core_lease_error(error)),
        };
        let session_binding = SessionBinding {
            lease_id: sender.ack.lease_id,
            process_generation: sender.ack.process_generation,
            listener_epoch: sender.ack.listener_epoch,
            nk_handshake_hash: sender.ack.nk_handshake_hash,
        };
        let (ack, lease) = match sender.write_ack() {
            Ok(value) => value,
            Err(error) => return Ok(self.core_lease_error(error)),
        };
        let write_token = allocate_token(&mut self.next_token)?;
        self.phase = Phase::BootstrapAckPending {
            stream_id,
            lease,
            pbs,
            session_binding,
            write_token,
            decoder,
            frame_timer,
        };
        let outcome = Outcome::write(stream_id, write_token, ack);
        Ok(match frame_deadline {
            Some(timer) => outcome.deadline(timer, FRAME_DEADLINE_MS),
            None => outcome,
        })
    }

    fn lease_bytes(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        let Phase::Eligible(eligible) = &mut self.phase else {
            return Err(SupervisorError::WrongPhase);
        };
        if eligible.lease_write.is_some() {
            return Err(SupervisorError::WrongPhase);
        }
        let frames = match eligible.lease_decoder.push(event.bytes) {
            Ok(value) => value,
            Err(error) => return Ok(self.core_lease_error(error)),
        };
        let frame_deadline = update_frame_timer(
            eligible.lease_decoder.is_incomplete(),
            &mut eligible.lease_frame_timer,
            &mut self.next_token,
            &mut self.timers,
            TimerAction::LeaseFrame,
        )?;
        if frames.is_empty() {
            let outcome = Outcome::new(OutcomeKind::NeedInput).stream(event.stream_id);
            return Ok(match frame_deadline {
                Some(timer) => outcome.deadline(timer, FRAME_DEADLINE_MS),
                None => outcome,
            });
        }
        if frames.len() != 1 {
            return Ok(self.lease_terminal(CloseReason::SequenceViolation, None));
        }
        let counter = match eligible.lease.read_heartbeat_request(&frames[0]) {
            Ok(value) => value,
            Err(error) => return Ok(self.core_lease_error(error)),
        };
        let reply = match eligible.lease.write_heartbeat_reply(counter) {
            Ok(value) => value,
            Err(error) => return Ok(self.core_lease_error(error)),
        };
        eligible.missed_ticks = 0;
        let token = allocate_token(&mut self.next_token)?;
        eligible.lease_write = Some((token, counter));
        let outcome = Outcome::write(event.stream_id, token, reply);
        Ok(match frame_deadline {
            Some(timer) => outcome.deadline(timer, FRAME_DEADLINE_MS),
            None => outcome,
        })
    }

    fn session_bytes(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        let Phase::Eligible(eligible) = &mut self.phase else {
            return Err(SupervisorError::WrongPhase);
        };
        let child = eligible
            .children
            .get_mut(&event.stream_id)
            .ok_or(SupervisorError::WrongPhase)?;
        let frames = match child.decoder.push(event.bytes) {
            Ok(value) => value,
            Err(error) => return Ok(self.child_core_error(event.stream_id, error)),
        };
        let frame_deadline = update_frame_timer(
            child.decoder.is_incomplete(),
            &mut child.frame_timer,
            &mut self.next_token,
            &mut self.timers,
            TimerAction::SessionFrame(event.stream_id),
        )?;
        if frames.is_empty() {
            let outcome = Outcome::new(OutcomeKind::NeedInput).stream(event.stream_id);
            return Ok(match frame_deadline {
                Some(timer) => outcome.deadline(timer, FRAME_DEADLINE_MS),
                None => outcome,
            });
        }
        let phase = std::mem::replace(&mut child.phase, ChildPhase::Terminal);
        match phase {
            ChildPhase::AwaitM2(mut session) => {
                if frames.len() != 1 {
                    return Ok(self.child_terminal(
                        event.stream_id,
                        CloseReason::SequenceViolation,
                        None,
                    ));
                }
                if let Err(error) = session.read_m2(&frames[0]) {
                    return Ok(self.child_core_error(event.stream_id, error));
                }
                let finished = match session.write_finished() {
                    Ok(value) => value,
                    Err(error) => return Ok(self.child_core_error(event.stream_id, error)),
                };
                let token = allocate_token(&mut self.next_token)?;
                child.phase = ChildPhase::FinishedPending {
                    session,
                    write_token: token,
                };
                let outcome = Outcome::write(event.stream_id, token, finished);
                Ok(match frame_deadline {
                    Some(timer) => outcome.deadline(timer, FRAME_DEADLINE_MS),
                    None => outcome,
                })
            }
            ChildPhase::AwaitFinished(mut session) => {
                if frames.len() != 1 {
                    return Ok(self.child_terminal(
                        event.stream_id,
                        CloseReason::SequenceViolation,
                        None,
                    ));
                }
                if let Err(error) = session.read_finished(&frames[0]) {
                    return Ok(self.child_core_error(event.stream_id, error));
                }
                child.phase = ChildPhase::AwaitApplication(session);
                if let Some(timer) = frame_deadline {
                    if let Some(phase_timer) = child.phase_timer.take() {
                        self.timers.remove(&phase_timer);
                    }
                    return Ok(Outcome::new(OutcomeKind::NeedInput)
                        .stream(event.stream_id)
                        .deadline(timer, FRAME_DEADLINE_MS));
                }
                reset_child_phase_timer(
                    eligible,
                    &mut self.next_token,
                    &mut self.timers,
                    event.stream_id,
                    SESSION_IDLE_DEADLINE_MS,
                )
            }
            ChildPhase::AwaitApplication(mut session) => {
                let mut application = None;
                for frame in frames {
                    match session.read_application(&frame) {
                        Ok(Some(bytes)) if application.is_none() => application = Some(bytes),
                        Ok(Some(_)) => {
                            return Ok(self.child_terminal(
                                event.stream_id,
                                CloseReason::SequenceViolation,
                                None,
                            ));
                        }
                        Ok(None) => {}
                        Err(error) => return Ok(self.child_core_error(event.stream_id, error)),
                    }
                }
                if let Some(bytes) = application {
                    child.applications = child
                        .applications
                        .checked_add(1)
                        .ok_or(SupervisorError::Internal)?;
                    child.phase = ChildPhase::AwaitRuntime(session);
                    let deadline = if child.applications == 1 {
                        SESSION_OPEN_RESPONSE_DEADLINE_MS
                    } else {
                        SESSION_IDLE_DEADLINE_MS
                    };
                    if let Some(timer) = frame_deadline {
                        if let Some(phase_timer) = child.phase_timer.take() {
                            self.timers.remove(&phase_timer);
                        }
                        Ok(Outcome::application(event.stream_id, bytes)
                            .deadline(timer, FRAME_DEADLINE_MS))
                    } else {
                        let timer = replace_child_timer(
                            eligible,
                            &mut self.next_token,
                            &mut self.timers,
                            event.stream_id,
                            TimerAction::SessionPhase(event.stream_id),
                        )?;
                        Ok(Outcome::application(event.stream_id, bytes).deadline(timer, deadline))
                    }
                } else {
                    child.phase = ChildPhase::AwaitApplication(session);
                    let outcome = Outcome::new(OutcomeKind::NeedInput).stream(event.stream_id);
                    Ok(match frame_deadline {
                        Some(timer) => outcome.deadline(timer, FRAME_DEADLINE_MS),
                        None => outcome,
                    })
                }
            }
            other => {
                child.phase = other;
                Err(SupervisorError::WrongPhase)
            }
        }
    }

    fn stream_closed(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        if event.stream_id == 0 || event.write_token != 0 || !event.bytes.is_empty() {
            return Err(SupervisorError::InvalidArgument);
        }
        let reason = match event.tag {
            EventTag::StreamIoFailed => CloseReason::PeerClosed,
            EventTag::StreamEof => CloseReason::PeerClosed,
            EventTag::StreamCloseNormal => CloseReason::Normal,
            _ => unreachable!(),
        };
        if !matches!(event.tag, EventTag::StreamIoFailed) {
            match &mut self.phase {
                Phase::BootstrapM1Pending {
                    stream_id, decoder, ..
                }
                | Phase::BootstrapM2 {
                    stream_id, decoder, ..
                }
                | Phase::BootstrapAckPending {
                    stream_id, decoder, ..
                } if *stream_id == event.stream_id => {
                    if let Err(error) = decoder.eof() {
                        return Ok(self.core_lease_error(error));
                    }
                }
                Phase::Eligible(eligible) if eligible.lease_stream == event.stream_id => {
                    if let Err(error) = eligible.lease_decoder.eof() {
                        return Ok(self.core_lease_error(error));
                    }
                }
                Phase::Eligible(eligible) => {
                    if let Some(child) = eligible.children.get_mut(&event.stream_id)
                        && let Some(error) = child.eof_error()
                    {
                        return Ok(self.child_core_error(event.stream_id, error));
                    }
                }
                _ => {}
            }
        }
        match &self.phase {
            Phase::BootstrapM1Pending { stream_id, .. }
            | Phase::BootstrapM2 { stream_id, .. }
            | Phase::BootstrapAckPending { stream_id, .. }
                if *stream_id == event.stream_id =>
            {
                Ok(self.lease_terminal(reason, None))
            }
            Phase::Eligible(eligible) if eligible.lease_stream == event.stream_id => {
                Ok(self.lease_terminal(CloseReason::BrokerLost, None))
            }
            Phase::Eligible(eligible) if eligible.children.contains_key(&event.stream_id) => {
                Ok(self.child_terminal(event.stream_id, reason, None))
            }
            _ => Err(SupervisorError::WrongPhase),
        }
    }

    fn timer_fired(&mut self, event: Event<'_>) -> Result<Outcome, SupervisorError> {
        if event.stream_id != 0 || event.write_token == 0 || !event.bytes.is_empty() {
            return Err(SupervisorError::InvalidArgument);
        }
        let Some(action) = self.timers.remove(&event.write_token) else {
            return Ok(Outcome::new(OutcomeKind::NeedInput));
        };
        match action {
            TimerAction::Bootstrap | TimerAction::BootstrapFrame => {
                Ok(self.lease_terminal(CloseReason::Timeout, None))
            }
            TimerAction::LeaseFrame => Ok(self.lease_terminal(CloseReason::Timeout, None)),
            TimerAction::LeaseTick => {
                let Phase::Eligible(eligible) = &mut self.phase else {
                    return Ok(Outcome::new(OutcomeKind::NeedInput));
                };
                if eligible.lease_tick_timer != Some(event.write_token) {
                    return Ok(Outcome::new(OutcomeKind::NeedInput));
                }
                eligible.lease_ticks = eligible.lease_ticks.saturating_add(1);
                eligible.missed_ticks = eligible.missed_ticks.saturating_add(1);
                if eligible.lease_ticks >= 30 {
                    return Ok(self.lease_terminal(CloseReason::Stale, None));
                }
                if eligible.missed_ticks >= 4 {
                    return Ok(self.lease_terminal(CloseReason::BrokerLost, None));
                }
                let timer = schedule_timer(
                    &mut self.next_token,
                    &mut self.timers,
                    TimerAction::LeaseTick,
                )?;
                eligible.lease_tick_timer = Some(timer);
                Ok(Outcome::new(OutcomeKind::NeedInput).deadline(timer, LEASE_TICK_MS))
            }
            TimerAction::SessionPhase(stream_id) | TimerAction::SessionFrame(stream_id) => {
                match &self.phase {
                    Phase::Eligible(eligible) if eligible.children.contains_key(&stream_id) => {
                        Ok(self.child_terminal(stream_id, CloseReason::Timeout, None))
                    }
                    _ => Ok(Outcome::new(OutcomeKind::NeedInput)),
                }
            }
            TimerAction::SessionIdle(stream_id) => match &self.phase {
                Phase::Eligible(eligible) if eligible.children.contains_key(&stream_id) => {
                    Ok(self.child_terminal(stream_id, CloseReason::Stale, None))
                }
                _ => Ok(Outcome::new(OutcomeKind::NeedInput)),
            },
        }
    }

    fn child_core_error(&mut self, stream_id: u64, mut error: super::Error) -> Outcome {
        let peer = error.peer_close_details();
        let reason = error.close_reason();
        if let Some(frame) = error.take_close_frame() {
            let handoff = match &self.phase {
                Phase::Eligible(eligible)
                    if eligible
                        .children
                        .get(&stream_id)
                        .is_some_and(|child| child.applications > 0) =>
                {
                    HandoffState::HandoffPossibleOrConfirmed
                }
                _ => HandoffState::NotHandedOff,
            };
            let Ok(token) = allocate_token(&mut self.next_token) else {
                return self.child_terminal(stream_id, CloseReason::InternalError, None);
            };
            if let Phase::Eligible(eligible) = &mut self.phase
                && let Some(child) = eligible.children.get_mut(&stream_id)
            {
                child.phase = ChildPhase::TerminalPending {
                    write_token: token,
                    terminal: Terminal {
                        reason,
                        handoff,
                        peer_close: peer,
                    },
                };
                let mut outcome = Outcome::write(stream_id, token, frame);
                outcome.flags |= 1 << 3;
                outcome.close_reason = reason;
                outcome.handoff = handoff;
                outcome.peer_close = peer;
                return outcome;
            }
        }
        self.child_terminal(stream_id, reason, peer)
    }

    fn core_lease_error(&mut self, error: super::Error) -> Outcome {
        let peer = error.peer_close_details();
        self.lease_terminal(error.close_reason(), peer)
    }

    fn child_terminal(
        &mut self,
        stream_id: u64,
        reason: CloseReason,
        peer: Option<(CloseReason, HandoffState)>,
    ) -> Outcome {
        let mut handoff = HandoffState::NotHandedOff;
        if let Phase::Eligible(eligible) = &mut self.phase
            && let Some(child) = eligible.children.remove(&stream_id)
        {
            if child.applications > 0 {
                handoff = HandoffState::HandoffPossibleOrConfirmed;
            }
            if let Some(timer) = child.phase_timer {
                self.timers.remove(&timer);
            }
            if let Some(timer) = child.frame_timer {
                self.timers.remove(&timer);
            }
        }
        Outcome::terminal(
            OutcomeKind::SessionTerminal,
            stream_id,
            reason,
            handoff,
            peer,
        )
    }

    fn lease_terminal(
        &mut self,
        reason: CloseReason,
        peer: Option<(CloseReason, HandoffState)>,
    ) -> Outcome {
        let handoff = match &self.phase {
            Phase::Eligible(eligible)
                if eligible
                    .children
                    .values()
                    .any(|child| child.applications > 0) =>
            {
                HandoffState::HandoffPossibleOrConfirmed
            }
            Phase::Terminal(terminal) => terminal.handoff,
            _ => HandoffState::NotHandedOff,
        };
        self.timers.clear();
        let terminal = Terminal {
            reason,
            handoff,
            peer_close: peer,
        };
        self.phase = Phase::Terminal(terminal);
        Outcome::terminal(OutcomeKind::LeaseTerminal, 0, reason, handoff, peer)
    }

    fn eligibility_lost(&mut self) -> Outcome {
        if let Phase::Eligible(eligible) = &mut self.phase {
            let Some(next_epoch) = eligible.session_binding.listener_epoch.checked_add(1) else {
                return self.lease_terminal(CloseReason::InternalError, None);
            };
            if next_epoch > JSON_SAFE_MAX {
                return self.lease_terminal(CloseReason::InternalError, None);
            }
            eligible.session_binding.listener_epoch = next_epoch;
        }
        self.lease_terminal(CloseReason::EligibilityLost, None)
    }

    fn cancel_action(&mut self, predicate: impl Fn(TimerAction) -> bool) {
        self.timers.retain(|_, action| !predicate(*action));
    }

    #[cfg(test)]
    fn force_child_last_close_nonce(&mut self, stream_id: u64) {
        let Phase::Eligible(eligible) = &mut self.phase else {
            panic!("lease not eligible");
        };
        let child = eligible.children.get_mut(&stream_id).expect("child");
        let ChildPhase::AwaitRuntime(session) = &mut child.phase else {
            panic!("child not awaiting runtime");
        };
        session.core.set_usage(super::RECORD_LIMIT - 2, 0, 0, 0);
    }
}

impl Child {
    fn eof_error(&mut self) -> Option<super::Error> {
        if let Err(error) = self.decoder.eof() {
            return Some(error);
        }
        let result = match &mut self.phase {
            ChildPhase::M1Pending { session, .. }
            | ChildPhase::AwaitM2(session)
            | ChildPhase::FinishedPending { session, .. }
            | ChildPhase::AwaitFinished(session)
            | ChildPhase::AwaitApplication(session)
            | ChildPhase::AwaitRuntime(session)
            | ChildPhase::ResponsePending { session, .. } => session.eof(),
            ChildPhase::TerminalPending { .. } | ChildPhase::Terminal => return None,
        };
        match result {
            Err(error) if error.close_reason() != CloseReason::PeerClosed => Some(error),
            _ => None,
        }
    }
}

impl Terminal {
    const fn internal() -> Self {
        Self {
            reason: CloseReason::InternalError,
            handoff: HandoffState::NotHandedOff,
            peer_close: None,
        }
    }
}

impl Outcome {
    fn new(kind: OutcomeKind) -> Self {
        Self {
            kind,
            flags: 0,
            stream_id: 0,
            write_token: 0,
            bytes: None,
            value0: 0,
            value1: 0,
            next_deadline_ms: 0,
            close_reason: CloseReason::Normal,
            handoff: HandoffState::NotHandedOff,
            peer_close: None,
        }
    }

    fn stream(mut self, stream_id: u64) -> Self {
        self.stream_id = stream_id;
        self
    }

    fn values(mut self, value0: u64, value1: u64) -> Self {
        self.value0 = value0;
        self.value1 = value1;
        self
    }

    fn deadline(mut self, timer: u64, milliseconds: u64) -> Self {
        if self.write_token == 0 {
            self.write_token = timer;
            self.flags |= 1 << 2;
        } else {
            self.value0 = timer;
            self.flags |= 1 << 1;
        }
        self.next_deadline_ms = milliseconds;
        self
    }

    fn with_deadline(self, timer: u64, milliseconds: u64) -> Self {
        self.deadline(timer, milliseconds)
    }

    fn write(stream_id: u64, write_token: u64, bytes: Vec<u8>) -> Self {
        let mut outcome = Self::new(OutcomeKind::WriteFrames).stream(stream_id);
        outcome.write_token = write_token;
        outcome.bytes = Some(bytes);
        outcome
    }

    fn application(stream_id: u64, bytes: Vec<u8>) -> Self {
        let mut outcome = Self::new(OutcomeKind::Application).stream(stream_id);
        outcome.handoff = HandoffState::HandoffPossibleOrConfirmed;
        outcome.bytes = Some(bytes);
        outcome
    }

    fn terminal(
        kind: OutcomeKind,
        stream_id: u64,
        reason: CloseReason,
        handoff: HandoffState,
        peer: Option<(CloseReason, HandoffState)>,
    ) -> Self {
        let mut outcome = Self::new(kind).stream(stream_id);
        outcome.close_reason = reason;
        outcome.handoff = handoff;
        outcome.peer_close = peer;
        outcome
    }
}

fn concat_frames(frames: Vec<Vec<u8>>) -> Vec<u8> {
    let capacity = frames.iter().map(Vec::len).sum();
    let mut output = Vec::with_capacity(capacity);
    for frame in frames {
        output.extend_from_slice(&frame);
    }
    output
}

fn allocate_token(next: &mut u64) -> Result<u64, SupervisorError> {
    let token = *next;
    *next = next.checked_add(1).ok_or(SupervisorError::Internal)?;
    if token == 0 {
        return Err(SupervisorError::Internal);
    }
    Ok(token)
}

fn schedule_timer(
    next: &mut u64,
    timers: &mut HashMap<u64, TimerAction>,
    action: TimerAction,
) -> Result<u64, SupervisorError> {
    let token = allocate_token(next)?;
    timers.insert(token, action);
    Ok(token)
}

fn update_frame_timer(
    incomplete: bool,
    slot: &mut Option<u64>,
    next: &mut u64,
    timers: &mut HashMap<u64, TimerAction>,
    action: TimerAction,
) -> Result<Option<u64>, SupervisorError> {
    if incomplete {
        if slot.is_none() {
            let timer = schedule_timer(next, timers, action)?;
            *slot = Some(timer);
            return Ok(Some(timer));
        }
    } else if let Some(timer) = slot.take() {
        timers.remove(&timer);
    }
    Ok(None)
}

fn replace_child_timer(
    eligible: &mut Eligible,
    next: &mut u64,
    timers: &mut HashMap<u64, TimerAction>,
    stream_id: u64,
    action: TimerAction,
) -> Result<u64, SupervisorError> {
    let old = eligible
        .children
        .get_mut(&stream_id)
        .ok_or(SupervisorError::WrongPhase)?
        .phase_timer
        .take();
    if let Some(old) = old {
        timers.remove(&old);
    }
    let timer = schedule_timer(next, timers, action)?;
    eligible
        .children
        .get_mut(&stream_id)
        .ok_or(SupervisorError::WrongPhase)?
        .phase_timer = Some(timer);
    Ok(timer)
}

fn reset_child_phase_timer(
    eligible: &mut Eligible,
    next: &mut u64,
    timers: &mut HashMap<u64, TimerAction>,
    stream_id: u64,
    deadline: u64,
) -> Result<Outcome, SupervisorError> {
    let timer = replace_child_timer(
        eligible,
        next,
        timers,
        stream_id,
        TimerAction::SessionIdle(stream_id),
    )?;
    Ok(Outcome::new(OutcomeKind::NeedInput)
        .stream(stream_id)
        .deadline(timer, deadline))
}

fn generate_process_generation() -> Result<u64, SupervisorError> {
    let mut rng = DefaultResolver
        .resolve_rng()
        .ok_or(SupervisorError::Internal)?;
    let modulus = JSON_SAFE_MAX;
    let zone = u64::MAX - (u64::MAX % modulus);
    loop {
        let mut bytes = [0_u8; 8];
        rng.try_fill_bytes(&mut bytes)
            .map_err(|_| SupervisorError::Internal)?;
        let value = u64::from_le_bytes(bytes);
        if value < zone {
            return Ok((value % modulus) + 1);
        }
    }
}

fn parse_descriptor(bytes: &[u8]) -> Result<ParsedDescriptor, SupervisorError> {
    if bytes.is_empty() || bytes.len() > DESCRIPTOR_MAX {
        return Err(SupervisorError::InvalidArgument);
    }
    let mut decoder = Decoder::new(bytes);
    if decoder
        .map()
        .map_err(|_| SupervisorError::InvalidArgument)?
        != Some(9)
    {
        return Err(SupervisorError::InvalidArgument);
    }
    expect_key(&mut decoder, 0)?;
    if decoder.u8().map_err(|_| SupervisorError::InvalidArgument)? != 1 {
        return Err(SupervisorError::InvalidArgument);
    }
    expect_key(&mut decoder, 1)?;
    let platform = decoder.u8().map_err(|_| SupervisorError::InvalidArgument)?;
    if !matches!(platform, 0 | 1) {
        return Err(SupervisorError::InvalidArgument);
    }
    expect_key(&mut decoder, 2)?;
    let lease_id = fixed_bytes::<16>(&mut decoder)?;
    expect_key(&mut decoder, 3)?;
    let target_nonce = fixed_bytes::<32>(&mut decoder)?;
    expect_key(&mut decoder, 4)?;
    let app_artifact_digest = fixed_bytes::<32>(&mut decoder)?;
    expect_key(&mut decoder, 5)?;
    let broker_public_key = fixed_bytes::<32>(&mut decoder)?;
    expect_key(&mut decoder, 6)?;
    let endpoint = parse_endpoint(&mut decoder, platform)?;
    expect_key(&mut decoder, 7)?;
    let expiry_ms = decoder
        .u64()
        .map_err(|_| SupervisorError::InvalidArgument)?;
    if !(1..=JSON_SAFE_MAX).contains(&expiry_ms) {
        return Err(SupervisorError::InvalidArgument);
    }
    expect_key(&mut decoder, 8)?;
    let target_reference_digest = fixed_bytes::<32>(&mut decoder)?;
    if decoder.position() != bytes.len() {
        return Err(SupervisorError::InvalidArgument);
    }
    let parsed = ParsedDescriptor {
        binding: BootstrapBinding {
            target_reference_digest,
            lease_id,
            target_nonce,
            app_artifact_digest,
            expiry_ms,
        },
        broker_public_key,
        endpoint,
    };
    if encode_descriptor(&parsed)? != bytes {
        return Err(SupervisorError::InvalidArgument);
    }
    Ok(parsed)
}

fn parse_endpoint(decoder: &mut Decoder<'_>, platform: u8) -> Result<Endpoint, SupervisorError> {
    match platform {
        0 => {
            if decoder
                .map()
                .map_err(|_| SupervisorError::InvalidArgument)?
                != Some(2)
            {
                return Err(SupervisorError::InvalidArgument);
            }
            expect_key(decoder, 0)?;
            if decoder
                .str()
                .map_err(|_| SupervisorError::InvalidArgument)?
                != "127.0.0.1"
            {
                return Err(SupervisorError::InvalidArgument);
            }
            expect_key(decoder, 1)?;
            let port = decoder
                .u16()
                .map_err(|_| SupervisorError::InvalidArgument)?;
            if !(49_152..=65_535).contains(&port) {
                return Err(SupervisorError::InvalidArgument);
            }
            Ok(Endpoint::Ios(port))
        }
        1 => {
            if decoder
                .map()
                .map_err(|_| SupervisorError::InvalidArgument)?
                != Some(1)
            {
                return Err(SupervisorError::InvalidArgument);
            }
            expect_key(decoder, 0)?;
            let name = decoder
                .str()
                .map_err(|_| SupervisorError::InvalidArgument)?;
            if !(32..=96).contains(&name.len()) || name.bytes().any(|byte| byte == 0) {
                return Err(SupervisorError::InvalidArgument);
            }
            Ok(Endpoint::Android(name.to_owned()))
        }
        _ => Err(SupervisorError::InvalidArgument),
    }
}

fn encode_descriptor(parsed: &ParsedDescriptor) -> Result<Vec<u8>, SupervisorError> {
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new(&mut bytes);
    encoder
        .map(9)
        .map_err(|_| SupervisorError::Internal)?
        .u8(0)
        .map_err(|_| SupervisorError::Internal)?
        .u8(1)
        .map_err(|_| SupervisorError::Internal)?
        .u8(1)
        .map_err(|_| SupervisorError::Internal)?;
    match &parsed.endpoint {
        Endpoint::Ios(_) => encoder.u8(0),
        Endpoint::Android(_) => encoder.u8(1),
    }
    .map_err(|_| SupervisorError::Internal)?;
    encoder
        .u8(2)
        .map_err(|_| SupervisorError::Internal)?
        .bytes(&parsed.binding.lease_id)
        .map_err(|_| SupervisorError::Internal)?
        .u8(3)
        .map_err(|_| SupervisorError::Internal)?
        .bytes(&parsed.binding.target_nonce)
        .map_err(|_| SupervisorError::Internal)?
        .u8(4)
        .map_err(|_| SupervisorError::Internal)?
        .bytes(&parsed.binding.app_artifact_digest)
        .map_err(|_| SupervisorError::Internal)?
        .u8(5)
        .map_err(|_| SupervisorError::Internal)?
        .bytes(&parsed.broker_public_key)
        .map_err(|_| SupervisorError::Internal)?
        .u8(6)
        .map_err(|_| SupervisorError::Internal)?;
    match &parsed.endpoint {
        Endpoint::Ios(port) => {
            encoder
                .map(2)
                .map_err(|_| SupervisorError::Internal)?
                .u8(0)
                .map_err(|_| SupervisorError::Internal)?
                .str("127.0.0.1")
                .map_err(|_| SupervisorError::Internal)?
                .u8(1)
                .map_err(|_| SupervisorError::Internal)?
                .u16(*port)
                .map_err(|_| SupervisorError::Internal)?;
        }
        Endpoint::Android(name) => {
            encoder
                .map(1)
                .map_err(|_| SupervisorError::Internal)?
                .u8(0)
                .map_err(|_| SupervisorError::Internal)?
                .str(name)
                .map_err(|_| SupervisorError::Internal)?;
        }
    }
    encoder
        .u8(7)
        .map_err(|_| SupervisorError::Internal)?
        .u64(parsed.binding.expiry_ms)
        .map_err(|_| SupervisorError::Internal)?
        .u8(8)
        .map_err(|_| SupervisorError::Internal)?
        .bytes(&parsed.binding.target_reference_digest)
        .map_err(|_| SupervisorError::Internal)?;
    Ok(bytes)
}

fn expect_key(decoder: &mut Decoder<'_>, key: u8) -> Result<(), SupervisorError> {
    if decoder.u8().map_err(|_| SupervisorError::InvalidArgument)? == key {
        Ok(())
    } else {
        Err(SupervisorError::InvalidArgument)
    }
}

fn fixed_bytes<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[u8; N], SupervisorError> {
    decoder
        .bytes()
        .map_err(|_| SupervisorError::InvalidArgument)?
        .try_into()
        .map_err(|_| SupervisorError::InvalidArgument)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BootstrapBinding, BrokerBootstrap, BrokerLeaseConnection, BrokerSession,
        BrokerStaticKeypair, ProcessBootstrapSecret, SessionBinding,
    };
    use minicbor::Encoder;

    fn descriptor(binding: &BootstrapBinding, public_key: [u8; 32], platform: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = Encoder::new(&mut bytes);
        encoder
            .map(9)
            .unwrap()
            .u8(0)
            .unwrap()
            .u8(1)
            .unwrap()
            .u8(1)
            .unwrap()
            .u8(platform)
            .unwrap()
            .u8(2)
            .unwrap()
            .bytes(&binding.lease_id)
            .unwrap()
            .u8(3)
            .unwrap()
            .bytes(&binding.target_nonce)
            .unwrap()
            .u8(4)
            .unwrap()
            .bytes(&binding.app_artifact_digest)
            .unwrap()
            .u8(5)
            .unwrap()
            .bytes(&public_key)
            .unwrap()
            .u8(6)
            .unwrap();
        if platform == 0 {
            encoder
                .map(2)
                .unwrap()
                .u8(0)
                .unwrap()
                .str("127.0.0.1")
                .unwrap()
                .u8(1)
                .unwrap()
                .u16(55_001)
                .unwrap();
        } else {
            encoder
                .map(1)
                .unwrap()
                .u8(0)
                .unwrap()
                .str("apppilotkit-0123456789abcdef0123456789abcdef")
                .unwrap();
        }
        encoder
            .u8(7)
            .unwrap()
            .u64(binding.expiry_ms)
            .unwrap()
            .u8(8)
            .unwrap()
            .bytes(&binding.target_reference_digest)
            .unwrap();
        bytes
    }

    fn binding() -> BootstrapBinding {
        BootstrapBinding {
            target_reference_digest: [0x41; 32],
            lease_id: [0x51; 16],
            target_nonce: [0x61; 32],
            app_artifact_digest: [0x71; 32],
            expiry_ms: 1_893_456_000_000,
        }
    }

    fn drive(
        target: &mut TargetTransport,
        tag: EventTag,
        stream_id: u64,
        write_token: u64,
        bytes: &[u8],
    ) -> Outcome {
        target
            .drive(Event {
                tag,
                flags: 0,
                stream_id,
                write_token,
                bytes,
            })
            .expect("valid supervisor event")
    }

    fn bootstrapped() -> (
        TargetTransport,
        BrokerLeaseConnection,
        ProcessBootstrapSecret,
        SessionBinding,
    ) {
        let binding = binding();
        let keypair = BrokerStaticKeypair::generate().expect("keypair");
        let public_key = keypair.public_key();
        let broker_pbs = ProcessBootstrapSecret::generate().expect("PBS");
        let broker = BrokerBootstrap::new(binding.clone(), keypair.into_private_key(), &broker_pbs)
            .expect("Broker bootstrap");
        let (mut target, _) =
            TargetTransport::create(&descriptor(&binding, public_key, 0)).expect("descriptor");
        let mut m1 = drive(&mut target, EventTag::BootstrapConnected, 7, 0, &[]);
        let m1_bytes = m1.bytes.take().expect("M1");
        drive(
            &mut target,
            EventTag::FullWriteCommitted,
            7,
            m1.write_token,
            &[],
        );
        let (m2, broker_ack) = broker.read_m1_write_m2(&m1_bytes).expect("M2");
        let mut ack = drive(&mut target, EventTag::StreamBytes, 7, 0, &m2);
        let ack_bytes = ack.bytes.take().expect("ACK");
        let (verified, broker_lease) = broker_ack.read_ack(&ack_bytes).expect("ACK verify");
        drive(
            &mut target,
            EventTag::FullWriteCommitted,
            7,
            ack.write_token,
            &[],
        );
        let session_binding = SessionBinding {
            lease_id: verified.lease_id,
            process_generation: verified.process_generation,
            listener_epoch: verified.listener_epoch,
            nk_handshake_hash: verified.nk_handshake_hash,
        };
        (target, broker_lease, broker_pbs, session_binding)
    }

    fn open_session(
        target: &mut TargetTransport,
        stream_id: u64,
        broker_pbs: &ProcessBootstrapSecret,
        binding: SessionBinding,
    ) -> BrokerSession {
        let m1 = drive(target, EventTag::SessionAccepted, stream_id, 0, &[]);
        assert_eq!(m1.kind, OutcomeKind::WriteFrames);
        assert_ne!(m1.flags & (1 << 1), 0, "handshake timer is explicit");
        let mut broker = BrokerSession::new(binding, broker_pbs).expect("Broker session");
        let m2 = broker
            .read_m1_write_m2(m1.bytes.as_deref().expect("M1 bytes"))
            .expect("M2");
        drive(
            target,
            EventTag::FullWriteCommitted,
            stream_id,
            m1.write_token,
            &[],
        );
        let finished = drive(target, EventTag::StreamBytes, stream_id, 0, &m2);
        broker
            .read_finished(finished.bytes.as_deref().expect("Target Finished"))
            .expect("Finished verify");
        drive(
            target,
            EventTag::FullWriteCommitted,
            stream_id,
            finished.write_token,
            &[],
        );
        let broker_finished = broker.write_finished().expect("Broker Finished");
        let ready = drive(
            target,
            EventTag::StreamBytes,
            stream_id,
            0,
            &broker_finished,
        );
        assert_eq!(ready.kind, OutcomeKind::NeedInput);
        broker
    }

    #[test]
    fn target_supervisor_completes_production_bootstrap_only_after_full_writes() {
        let binding = binding();
        let keypair = BrokerStaticKeypair::generate().expect("keypair");
        let public_key = keypair.public_key();
        let pbs = ProcessBootstrapSecret::generate().expect("PBS");
        let broker = BrokerBootstrap::new(binding.clone(), keypair.into_private_key(), &pbs)
            .expect("Broker bootstrap");
        let (mut target, ready) =
            TargetTransport::create(&descriptor(&binding, public_key, 0)).expect("descriptor");
        assert_eq!(ready.kind, OutcomeKind::EndpointReady);
        assert_eq!(ready.next_deadline_ms, 0);

        let mut m1 = target
            .drive(Event {
                tag: EventTag::BootstrapConnected,
                flags: 0,
                stream_id: 7,
                write_token: 0,
                bytes: &[],
            })
            .expect("M1 output");
        assert_eq!(m1.kind, OutcomeKind::WriteFrames);
        assert_ne!(m1.write_token, 0);
        assert_eq!(m1.next_deadline_ms, BOOTSTRAP_DEADLINE_MS);
        assert_ne!(m1.flags & (1 << 1), 0, "bootstrap timer is explicit");
        assert_eq!(
            target.drive(Event {
                tag: EventTag::StreamBytes,
                flags: 0,
                stream_id: 7,
                write_token: 0,
                bytes: &[0],
            }),
            Err(SupervisorError::WrongPhase),
        );
        let m1_bytes = m1.bytes.take().expect("M1 bytes");
        let committed = target
            .drive(Event {
                tag: EventTag::FullWriteCommitted,
                flags: 0,
                stream_id: 7,
                write_token: m1.write_token,
                bytes: &[],
            })
            .expect("M1 committed");
        assert_eq!(committed.kind, OutcomeKind::NeedInput);

        let (m2, broker_ack) = broker.read_m1_write_m2(&m1_bytes).expect("Broker M2");
        let mut ack = target
            .drive(Event {
                tag: EventTag::StreamBytes,
                flags: 0,
                stream_id: 7,
                write_token: 0,
                bytes: &m2,
            })
            .expect("Target ACK");
        assert_eq!(ack.kind, OutcomeKind::WriteFrames);
        let ack_bytes = ack.bytes.take().expect("ACK bytes");
        let (verified, _) = broker_ack.read_ack(&ack_bytes).expect("verified ACK");
        let lease_ready = target
            .drive(Event {
                tag: EventTag::FullWriteCommitted,
                flags: 0,
                stream_id: 7,
                write_token: ack.write_token,
                bytes: &[],
            })
            .expect("ACK committed");
        assert_eq!(lease_ready.kind, OutcomeKind::LeaseReady);
        assert_eq!(lease_ready.value0, verified.process_generation);
        assert_eq!(lease_ready.value1, 1);
        assert_ne!(
            lease_ready.write_token, 0,
            "lease tick deadline token is explicit"
        );
        assert_ne!(
            lease_ready.flags & (1 << 2),
            0,
            "lease tick deadline token is carried by write_token"
        );
        assert_eq!(
            lease_ready.flags & (1 << 1),
            0,
            "lease generation remains in value0 rather than holding the deadline token"
        );
        assert_ne!(lease_ready.write_token, lease_ready.value0);
        let next_tick = drive(
            &mut target,
            EventTag::TimerFired,
            0,
            lease_ready.write_token,
            &[],
        );
        assert_eq!(next_tick.kind, OutcomeKind::NeedInput);
        assert_ne!(next_tick.flags & (1 << 2), 0);
    }

    #[test]
    fn bootstrap_eof_with_partial_outer_frame_is_malformed() {
        let keypair = BrokerStaticKeypair::generate().expect("keypair");
        let (mut target, _) =
            TargetTransport::create(&descriptor(&binding(), keypair.public_key(), 0))
                .expect("descriptor");
        let m1 = drive(&mut target, EventTag::BootstrapConnected, 7, 0, &[]);
        drive(
            &mut target,
            EventTag::FullWriteCommitted,
            7,
            m1.write_token,
            &[],
        );
        assert_eq!(
            drive(&mut target, EventTag::StreamBytes, 7, 0, &[0]).kind,
            OutcomeKind::NeedInput
        );
        let terminal = drive(&mut target, EventTag::StreamEof, 7, 0, &[]);
        assert_eq!(terminal.kind, OutcomeKind::LeaseTerminal);
        assert_eq!(terminal.close_reason, CloseReason::Malformed);
    }

    #[test]
    fn descriptor_must_be_canonical_and_platform_endpoint_must_match() {
        let keypair = BrokerStaticKeypair::generate().expect("keypair");
        let mut valid = descriptor(&binding(), keypair.public_key(), 0);
        assert!(TargetTransport::create(&valid).is_ok());
        valid.push(0);
        assert_eq!(
            TargetTransport::create(&valid).err(),
            Some(SupervisorError::InvalidArgument)
        );

        let invalid_android = descriptor(&binding(), keypair.public_key(), 2);
        assert_eq!(
            TargetTransport::create(&invalid_android).err(),
            Some(SupervisorError::InvalidArgument)
        );
    }

    #[test]
    fn supervisor_owns_nnpsk0_handoff_response_and_heartbeat_lifecycles() {
        let (mut target, mut broker_lease, broker_pbs, session_binding) = bootstrapped();
        let heartbeat = broker_lease
            .write_heartbeat_request(1)
            .expect("heartbeat request");
        let reply = drive(&mut target, EventTag::StreamBytes, 7, 0, &heartbeat);
        assert_eq!(reply.kind, OutcomeKind::WriteFrames);
        assert_eq!(
            broker_lease
                .read_heartbeat_reply(reply.bytes.as_deref().expect("heartbeat reply"))
                .expect("reply verify"),
            1
        );
        drive(
            &mut target,
            EventTag::FullWriteCommitted,
            7,
            reply.write_token,
            &[],
        );

        let mut broker = open_session(&mut target, 11, &broker_pbs, session_binding);
        let open = broker
            .write_session_open(b"opaque session.open")
            .expect("open frames");
        let application = drive(
            &mut target,
            EventTag::StreamBytes,
            11,
            0,
            &concat_frames(open),
        );
        assert_eq!(application.kind, OutcomeKind::Application);
        assert_eq!(
            application.bytes.as_deref(),
            Some(b"opaque session.open".as_slice())
        );
        assert_eq!(
            application.handoff,
            HandoffState::HandoffPossibleOrConfirmed
        );
        let response = drive(
            &mut target,
            EventTag::RuntimeResponse,
            11,
            0,
            b"opaque response",
        );
        assert_eq!(response.kind, OutcomeKind::WriteFrames);
        let response_frames = OuterFrameDecoder::new()
            .push(response.bytes.as_deref().expect("response frames"))
            .expect("outer response");
        assert_eq!(response_frames.len(), 1);
        assert_eq!(
            broker
                .read_application_response(&response_frames[0])
                .expect("response decrypt"),
            Some(b"opaque response".to_vec())
        );
        drive(
            &mut target,
            EventTag::FullWriteCommitted,
            11,
            response.write_token,
            &[],
        );
    }

    #[test]
    fn lease_eof_with_partial_outer_frame_is_malformed() {
        let (mut target, mut broker_lease, _broker_pbs, _binding) = bootstrapped();
        let heartbeat = broker_lease
            .write_heartbeat_request(1)
            .expect("heartbeat request");
        assert!(heartbeat.len() > 1);
        assert_eq!(
            drive(&mut target, EventTag::StreamBytes, 7, 0, &heartbeat[..1],).kind,
            OutcomeKind::NeedInput
        );
        let terminal = drive(&mut target, EventTag::StreamCloseNormal, 7, 0, &[]);
        assert_eq!(terminal.kind, OutcomeKind::LeaseTerminal);
        assert_eq!(terminal.close_reason, CloseReason::Malformed);
    }

    #[test]
    fn coalesced_complete_and_partial_outer_frame_keeps_frame_deadline() {
        let (mut target, mut broker_lease, _broker_pbs, _binding) = bootstrapped();
        let mut heartbeat = broker_lease
            .write_heartbeat_request(1)
            .expect("heartbeat request");
        heartbeat.push(0);
        let reply = drive(&mut target, EventTag::StreamBytes, 7, 0, &heartbeat);
        assert_eq!(reply.kind, OutcomeKind::WriteFrames);
        assert_eq!(reply.next_deadline_ms, FRAME_DEADLINE_MS);
        assert_ne!(reply.flags & (1 << 1), 0);
        let terminal = drive(&mut target, EventTag::TimerFired, 0, reply.value0, &[]);
        assert_eq!(terminal.kind, OutcomeKind::LeaseTerminal);
        assert_eq!(terminal.close_reason, CloseReason::Timeout);
    }

    #[test]
    fn d0_session_idle_expiry_literal_maps_to_stale_not_timeout() {
        let vector: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/lifecycle-dispatch.json"
        )))
        .expect("D0 lifecycle vector");
        let case = vector["vectors"]
            .as_array()
            .expect("vectors")
            .iter()
            .find(|case| case["id"] == "session-idle-expired")
            .expect("session-idle-expired literal");
        assert_eq!(case["canonical_input"]["idle_ms"], SESSION_IDLE_DEADLINE_MS);
        assert_eq!(case["expected_close_reason"], "stale");

        let (mut target, _broker_lease, broker_pbs, binding) = bootstrapped();
        let mut broker = open_session(&mut target, 11, &broker_pbs, binding);
        let open = broker.write_session_open(b"open").expect("open");
        let application = drive(
            &mut target,
            EventTag::StreamBytes,
            11,
            0,
            &concat_frames(open),
        );
        let response = drive(&mut target, EventTag::RuntimeResponse, 11, 0, b"response");
        assert_eq!(
            application.next_deadline_ms,
            SESSION_OPEN_RESPONSE_DEADLINE_MS
        );
        let idle = drive(
            &mut target,
            EventTag::FullWriteCommitted,
            11,
            response.write_token,
            &[],
        );
        assert_eq!(idle.next_deadline_ms, SESSION_IDLE_DEADLINE_MS);
        let terminal = drive(&mut target, EventTag::TimerFired, 0, idle.write_token, &[]);
        assert_eq!(terminal.kind, OutcomeKind::SessionTerminal);
        assert_eq!(terminal.close_reason, CloseReason::Stale);
    }

    #[test]
    fn child_eof_with_incomplete_record_reassembly_is_malformed() {
        let (mut target, _broker_lease, broker_pbs, binding) = bootstrapped();
        let mut broker = open_session(&mut target, 11, &broker_pbs, binding);
        let open = broker
            .write_session_open(&vec![0xA5; 65_536])
            .expect("fragmented session.open");
        assert!(open.len() > 1);
        assert_eq!(
            drive(&mut target, EventTag::StreamBytes, 11, 0, &open[0],).kind,
            OutcomeKind::NeedInput
        );
        let terminal = drive(&mut target, EventTag::StreamEof, 11, 0, &[]);
        assert_eq!(terminal.kind, OutcomeKind::SessionTerminal);
        assert_eq!(terminal.close_reason, CloseReason::Malformed);
    }

    #[test]
    fn child_authentication_failure_isolated_while_lease_and_sibling_continue() {
        let (mut target, mut broker_lease, broker_pbs, binding) = bootstrapped();
        let mut sibling = open_session(&mut target, 11, &broker_pbs, binding.clone());

        let bad_m1 = drive(&mut target, EventTag::SessionAccepted, 12, 0, &[]);
        drive(
            &mut target,
            EventTag::FullWriteCommitted,
            12,
            bad_m1.write_token,
            &[],
        );
        let wrong_pbs = ProcessBootstrapSecret::new([0xEE; 32]);
        let mut wrong_broker = BrokerSession::new(binding, &wrong_pbs).expect("wrong Broker");
        let error = wrong_broker
            .read_m1_write_m2(bad_m1.bytes.as_deref().expect("M1"))
            .expect_err("wrong PBS");
        assert_eq!(error.close_reason(), CloseReason::AuthenticationFailed);
        let terminal = drive(&mut target, EventTag::StreamIoFailed, 12, 0, &[]);
        assert_eq!(terminal.kind, OutcomeKind::SessionTerminal);

        let open = sibling.write_session_open(b"sibling open").expect("open");
        assert_eq!(
            drive(
                &mut target,
                EventTag::StreamBytes,
                11,
                0,
                &concat_frames(open),
            )
            .bytes
            .as_deref(),
            Some(b"sibling open".as_slice())
        );
        let heartbeat = broker_lease.write_heartbeat_request(1).expect("heartbeat");
        assert_eq!(
            drive(&mut target, EventTag::StreamBytes, 7, 0, &heartbeat).kind,
            OutcomeKind::WriteFrames
        );
    }

    #[test]
    fn close_immediately_drops_lease_pbs_noise_and_children() {
        let (mut target, _broker_lease, broker_pbs, binding) = bootstrapped();
        let _broker = open_session(&mut target, 11, &broker_pbs, binding);
        let before = super::super::secret_drop_count();
        let closed = target.close();
        assert_eq!(closed.kind, OutcomeKind::Closed);
        assert_eq!(
            super::super::secret_drop_count(),
            before + 1,
            "Target-owned lease PBS must drop before close returns"
        );
        let late = drive(&mut target, EventTag::SessionAccepted, 12, 0, &[]);
        assert_eq!(late.kind, OutcomeKind::LeaseTerminal);
    }

    #[test]
    fn internal_error_event_terminalizes_every_lease_phase_without_a_peer_frame() {
        let contract = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/README.md"
        ));
        assert!(contract.contains(
            "| invariant or non-peer implementation failure | `internalError` | `internalError`, exit 1 |"
        ));

        let keypair = BrokerStaticKeypair::generate().expect("keypair");
        let (mut awaiting_bootstrap, _) =
            TargetTransport::create(&descriptor(&binding(), keypair.public_key(), 0))
                .expect("descriptor");
        let terminal = drive(&mut awaiting_bootstrap, EventTag::InternalError, 0, 0, &[]);
        assert_eq!(terminal.kind, OutcomeKind::LeaseTerminal);
        assert_eq!(terminal.close_reason, CloseReason::InternalError);
        assert!(terminal.bytes.is_none());
        assert!(terminal.peer_close.is_none());

        let (mut eligible, _broker_lease, broker_pbs, binding) = bootstrapped();
        let _broker = open_session(&mut eligible, 11, &broker_pbs, binding);
        let before = super::super::secret_drop_count();
        let terminal = drive(&mut eligible, EventTag::InternalError, 0, 0, &[]);
        assert_eq!(terminal.kind, OutcomeKind::LeaseTerminal);
        assert_eq!(terminal.close_reason, CloseReason::InternalError);
        assert!(terminal.bytes.is_none());
        assert!(terminal.peer_close.is_none());
        assert_eq!(
            super::super::secret_drop_count(),
            before + 1,
            "Target-owned lease PBS must drop before the event returns"
        );
        let late = drive(&mut eligible, EventTag::SessionAccepted, 12, 0, &[]);
        assert_eq!(late.kind, OutcomeKind::LeaseTerminal);
        assert_eq!(late.close_reason, CloseReason::InternalError);
    }

    #[test]
    fn bootstrap_deadline_starts_only_after_the_host_connection_is_accepted() {
        let keypair = BrokerStaticKeypair::generate().expect("keypair");
        let (mut target, ready) =
            TargetTransport::create(&descriptor(&binding(), keypair.public_key(), 0))
                .expect("descriptor");
        assert_eq!(ready.next_deadline_ms, 0);
        assert_eq!(
            target.drive(Event {
                tag: EventTag::TimerFired,
                flags: 0,
                stream_id: 0,
                write_token: 1,
                bytes: &[],
            }),
            Ok(Outcome::new(OutcomeKind::NeedInput)),
            "a slow platform launch has no bootstrap timer to consume"
        );

        let m1 = drive(&mut target, EventTag::BootstrapConnected, 99, 0, &[]);
        assert_eq!(m1.next_deadline_ms, BOOTSTRAP_DEADLINE_MS);
        assert_ne!(m1.flags & (1 << 1), 0);
        let terminal = drive(&mut target, EventTag::TimerFired, 0, m1.value0, &[]);
        assert_eq!(terminal.kind, OutcomeKind::LeaseTerminal);
        assert_eq!(terminal.close_reason, CloseReason::Timeout);
        let late = drive(&mut target, EventTag::BootstrapConnected, 99, 0, &[]);
        assert_eq!(late.kind, OutcomeKind::LeaseTerminal);
        assert_eq!(late.close_reason, CloseReason::Timeout);
    }

    #[test]
    fn d0_descriptor_literal_is_accepted_without_self_generation() {
        let vector: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/bootstrap-nk-success.json"
        )))
        .expect("D0 bootstrap vector");
        let hex = vector["canonical_input"]["launch_descriptor_cbor_hex"]
            .as_str()
            .expect("descriptor literal");
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex"))
            .collect::<Vec<_>>();
        let (_, ready) = TargetTransport::create(&bytes).expect("accepted iOS descriptor");
        assert_eq!(ready.kind, OutcomeKind::EndpointReady);
        assert_eq!(ready.value0, 0);
        assert_eq!(ready.value1, 55_001);
        assert!(ready.bytes.is_none());
        let mut noncanonical = bytes;
        noncanonical.push(0);
        assert_eq!(
            TargetTransport::create(&noncanonical).err(),
            Some(SupervisorError::InvalidArgument)
        );

        let android: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/bootstrap-android-descriptor.json"
        )))
        .expect("D0 Android descriptor vector");
        let hex = android["canonical_input"]["launch_descriptor_cbor_hex"]
            .as_str()
            .expect("Android descriptor literal");
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex"))
            .collect::<Vec<_>>();
        let expected = android["canonical_input"]["launch_endpoint"]["localabstract_name"]
            .as_str()
            .expect("localabstract literal");
        let (_, ready) = TargetTransport::create(&bytes).expect("accepted Android descriptor");
        assert_eq!(ready.kind, OutcomeKind::EndpointReady);
        assert_eq!(ready.value0, 1);
        assert_eq!(ready.value1, 0);
        assert_eq!(ready.bytes.as_deref(), Some(expected.as_bytes()));
        assert!(!ready.bytes.as_deref().expect("endpoint bytes").contains(&0));
    }

    #[test]
    fn arbitrary_event_and_chunk_sequences_never_panic_or_revive_terminal_state() {
        let keypair = BrokerStaticKeypair::generate().expect("keypair");
        let descriptor = descriptor(&binding(), keypair.public_key(), 0);
        for seed in 1_u64..=64 {
            let (mut target, _) = TargetTransport::create(&descriptor).expect("descriptor");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut state = seed;
                for step in 0_u64..32 {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    let tag = match state % 12 {
                        0 => EventTag::BootstrapConnected,
                        1 => EventTag::StreamBytes,
                        2 => EventTag::FullWriteCommitted,
                        3 => EventTag::SessionAccepted,
                        4 => EventTag::RuntimeResponse,
                        5 => EventTag::StreamEof,
                        6 => EventTag::StreamIoFailed,
                        7 => EventTag::StreamCloseNormal,
                        8 => EventTag::TimerFired,
                        9 => EventTag::EligibilityLost,
                        10 => EventTag::CleanupFailed,
                        _ => EventTag::InternalError,
                    };
                    let bytes = state.to_le_bytes();
                    let length = (state as usize) % (bytes.len() + 1);
                    let _ = target.drive(Event {
                        tag,
                        flags: 0,
                        stream_id: (state >> 8) % 5,
                        write_token: (state >> 16) % 9,
                        bytes: &bytes[..length],
                    });
                    if step == 16 {
                        let _ = target.drive(Event {
                            tag: EventTag::EligibilityLost,
                            flags: 0,
                            stream_id: 0,
                            write_token: 0,
                            bytes: &[],
                        });
                    }
                }
            }));
            assert!(result.is_ok(), "sequence seed {seed} panicked");
            let terminal = drive(&mut target, EventTag::EligibilityLost, 0, 0, &[]);
            assert_eq!(terminal.kind, OutcomeKind::LeaseTerminal);
        }
    }

    #[test]
    fn record_limit_close_is_an_ordered_write_batch_before_child_terminal() {
        let (mut target, _broker_lease, broker_pbs, binding) = bootstrapped();
        let mut broker = open_session(&mut target, 11, &broker_pbs, binding);
        let open = broker.write_session_open(b"open").expect("open");
        let application = drive(
            &mut target,
            EventTag::StreamBytes,
            11,
            0,
            &concat_frames(open),
        );
        assert_eq!(application.kind, OutcomeKind::Application);
        target.force_child_last_close_nonce(11);
        let close = drive(&mut target, EventTag::RuntimeResponse, 11, 0, b"response");
        assert_eq!(close.kind, OutcomeKind::WriteFrames);
        assert_ne!(close.flags & (1 << 3), 0);
        assert_eq!(close.close_reason, CloseReason::RecordLimit);
        assert_eq!(
            broker
                .read_close(close.bytes.as_deref().expect("recordLimit Close"))
                .expect("authenticated Close"),
            (
                CloseReason::RecordLimit,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );
        let terminal = drive(
            &mut target,
            EventTag::FullWriteCommitted,
            11,
            close.write_token,
            &[],
        );
        assert_eq!(terminal.kind, OutcomeKind::SessionTerminal);
        assert_eq!(terminal.close_reason, CloseReason::RecordLimit);
    }
}
