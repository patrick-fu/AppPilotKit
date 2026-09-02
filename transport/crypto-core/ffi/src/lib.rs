//! Stable C ABI for the Rust-owned Target transport supervisor.

use apppilotkit_transport_crypto_core::CloseReason;
use apppilotkit_transport_crypto_core::target_transport::{
    Event, EventTag, Outcome, OutcomeKind, SupervisorError, TargetTransport,
};
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use zeroize::Zeroize;

#[cfg(test)]
static ZEROIZED_OUTPUT_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
type DriveAfterCloneHook = Option<(u64, Arc<std::sync::Barrier>)>;
#[cfg(test)]
static DRIVE_AFTER_CLONE_HOOK: OnceLock<Mutex<DriveAfterCloneHook>> = OnceLock::new();
#[cfg(test)]
type DriveAfterArbitrationHook = Option<(u64, Arc<std::sync::Barrier>)>;
#[cfg(test)]
static DRIVE_AFTER_ARBITRATION_HOOK: OnceLock<Mutex<DriveAfterArbitrationHook>> = OnceLock::new();
#[cfg(test)]
static CONCURRENCY_HOOK_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(any(target_os = "android", test, apppilotkit_jni_smoke))]
mod jni;

pub const ABI_VERSION: u32 = 0x0001_0000;
const MAX_DESCRIPTOR_BYTES: u64 = 8_192;
const MAX_EVENT_BYTES: u64 = 67_108_864;
const MAX_STREAM_CHUNK_BYTES: u64 = 1_048_576;

pub const STATUS_OK: i32 = 0;
pub const STATUS_NEED_INPUT: i32 = 1;
pub const STATUS_EVENT: i32 = 2;
pub const STATUS_TERMINAL: i32 = 3;
pub const STATUS_ABI_MISMATCH: i32 = -1;
pub const STATUS_INVALID_ARGUMENT: i32 = -2;
pub const STATUS_INVALID_HANDLE: i32 = -3;
pub const STATUS_WRONG_PHASE: i32 = -4;
pub const STATUS_BUSY: i32 = -5;
pub const STATUS_BUFFER_TOO_SMALL: i32 = -6;
pub const STATUS_INTERNAL_PANIC: i32 = -7;

const EVENT_BOOTSTRAP_CONNECTED: u32 = 1;
const EVENT_STREAM_BYTES: u32 = 2;
const EVENT_FULL_WRITE_COMMITTED: u32 = 3;
const EVENT_SESSION_ACCEPTED: u32 = 4;
const EVENT_RUNTIME_RESPONSE: u32 = 5;
const EVENT_STREAM_EOF: u32 = 6;
const EVENT_STREAM_IO_FAILED: u32 = 7;
const EVENT_STREAM_CLOSE_NORMAL: u32 = 8;
const EVENT_TIMER_FIRED: u32 = 9;
const EVENT_ELIGIBILITY_LOST: u32 = 10;
const EVENT_CLEANUP_FAILED: u32 = 11;
const EVENT_INTERNAL_ERROR: u32 = 12;

const OUTCOME_ENDPOINT_READY: u32 = 1;
const OUTCOME_WRITE_FRAMES: u32 = 2;
const OUTCOME_APPLICATION: u32 = 3;
const OUTCOME_LEASE_READY: u32 = 4;
const OUTCOME_NEED_INPUT: u32 = 5;
const OUTCOME_SESSION_TERMINAL: u32 = 6;
const OUTCOME_LEASE_TERMINAL: u32 = 7;
const OUTCOME_CLOSED: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CreateInputV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub descriptor_cbor: *const u8,
    pub descriptor_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub tag: u32,
    pub flags: u32,
    pub stream_id: u64,
    pub write_token: u64,
    pub bytes: *const u8,
    pub bytes_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutcomeV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub kind: u32,
    pub flags: u32,
    pub stream_id: u64,
    pub write_token: u64,
    pub output: u64,
    pub value0: u64,
    pub value1: u64,
    pub next_deadline_ms: u64,
    pub close_reason: u32,
    pub handoff_state: u32,
    pub peer_close_reason: u32,
    pub peer_handoff_state: u32,
    pub reserved: [u64; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StructHeaderV1 {
    abi_version: u32,
    struct_size: u32,
}

impl OutcomeV1 {
    const fn zeroed() -> Self {
        Self {
            abi_version: 0,
            struct_size: 0,
            kind: 0,
            flags: 0,
            stream_id: 0,
            write_token: 0,
            output: 0,
            value0: 0,
            value1: 0,
            next_deadline_ms: 0,
            close_reason: 0,
            handoff_state: 0,
            peer_close_reason: 0,
            peer_handoff_state: 0,
            reserved: [0; 4],
        }
    }
}

struct SupervisorEntry {
    busy: AtomicBool,
    retired: AtomicBool,
    pending_terminal: AtomicU8,
    pending_timer: AtomicU64,
    lease_stream: AtomicU64,
    publish_gate: Mutex<()>,
    terminal_timers: Mutex<HashSet<u64>>,
    transport: Mutex<TargetTransport>,
}

impl SupervisorEntry {
    fn new(transport: TargetTransport) -> Self {
        let terminal_timers = transport.terminal_timer_tokens().collect();
        Self {
            busy: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            pending_terminal: AtomicU8::new(0),
            pending_timer: AtomicU64::new(0),
            lease_stream: AtomicU64::new(0),
            publish_gate: Mutex::new(()),
            terminal_timers: Mutex::new(terminal_timers),
            transport: Mutex::new(transport),
        }
    }

    fn lock(&self) -> MutexGuard<'_, TargetTransport> {
        self.transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn terminate_internal(&self) {
        self.lock().terminate_internal_error();
    }

    fn refresh_terminal_timers(&self, transport: &TargetTransport) {
        let mut timers = self
            .terminal_timers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        timers.clear();
        timers.extend(transport.terminal_timer_tokens());
    }

    fn terminal_timer_is_active(&self, token: u64) -> bool {
        self.terminal_timers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&token)
    }
}

fn arbitrate_pending_events(
    entry: &SupervisorEntry,
    current: EventTag,
    terminal_on_entry: Option<CloseReason>,
    outcome: Result<Outcome, SupervisorError>,
) -> Result<Outcome, SupervisorError> {
    let pending = entry.pending_terminal.swap(0, Ordering::AcqRel);
    let current_already_applied = match (&outcome, pending, current) {
        (Ok(value), 1, EventTag::EligibilityLost) => {
            value.kind == OutcomeKind::LeaseTerminal
                && value.close_reason == CloseReason::EligibilityLost
        }
        (Ok(value), 2, EventTag::CleanupFailed) => {
            value.kind == OutcomeKind::LeaseTerminal
                && value.close_reason == CloseReason::CleanupFailed
        }
        (Ok(value), 4, EventTag::InternalError) => {
            value.kind == OutcomeKind::LeaseTerminal
                && value.close_reason == CloseReason::InternalError
        }
        _ => false,
    };
    if pending != 0 && !current_already_applied {
        let (tag, stream_id) = match pending {
            1 => (EventTag::EligibilityLost, 0),
            2 => (EventTag::CleanupFailed, 0),
            3 => (
                EventTag::StreamIoFailed,
                entry.lease_stream.load(Ordering::Acquire),
            ),
            4 => (EventTag::InternalError, 0),
            _ => (EventTag::CleanupFailed, 0),
        };
        let mut transport = entry.lock();
        if pending == 4 && terminal_on_entry.is_none() {
            transport.terminate_internal_error();
        }
        let result = transport.drive(Event {
            tag,
            flags: 0,
            stream_id,
            write_token: 0,
            bytes: &[],
        });
        entry.refresh_terminal_timers(&transport);
        return result;
    }
    let timer = entry.pending_timer.swap(0, Ordering::AcqRel);
    if timer == 0 || matches!(current, EventTag::TimerFired) {
        return outcome;
    }
    let mut transport = entry.lock();
    let timer_outcome = transport.drive(Event {
        tag: EventTag::TimerFired,
        flags: 0,
        stream_id: 0,
        write_token: timer,
        bytes: &[],
    });
    entry.refresh_terminal_timers(&transport);
    match timer_outcome {
        Ok(value)
            if matches!(
                value.kind,
                OutcomeKind::SessionTerminal | OutcomeKind::LeaseTerminal
            ) =>
        {
            Ok(value)
        }
        Ok(_) => outcome,
        Err(error) => Err(error),
    }
}

struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct OwnedOutput(Vec<u8>);

impl Drop for OwnedOutput {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        {
            assert!(self.0.iter().all(|byte| *byte == 0));
            ZEROIZED_OUTPUT_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct OutputEntry {
    owner: u64,
    bytes: OwnedOutput,
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

struct Registry<T> {
    slots: Vec<Slot<T>>,
}

impl<T> Registry<T> {
    const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    fn insert(&mut self, value: T) -> Result<u64, ()> {
        if let Some((index, slot)) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.value.is_none())
        {
            slot.value = Some(value);
            return encode_handle(index, slot.generation);
        }
        let index = self.slots.len();
        self.slots.push(Slot {
            generation: 1,
            value: Some(value),
        });
        encode_handle(index, 1)
    }

    fn get(&self, handle: u64) -> Option<&T> {
        let (index, generation) = decode_handle(handle)?;
        let slot = self.slots.get(index)?;
        (slot.generation == generation)
            .then_some(slot.value.as_ref())
            .flatten()
    }

    fn remove(&mut self, handle: u64) -> Option<T> {
        let (index, generation) = decode_handle(handle)?;
        let slot = self.slots.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        let value = slot.value.take()?;
        slot.generation = slot.generation.wrapping_add(1);
        if slot.generation == 0 {
            slot.generation = 1;
        }
        Some(value)
    }
}

fn encode_handle(index: usize, generation: u32) -> Result<u64, ()> {
    let slot = u32::try_from(index)
        .map_err(|_| ())?
        .checked_add(1)
        .ok_or(())?;
    Ok((u64::from(generation) << 32) | u64::from(slot))
}

fn decode_handle(handle: u64) -> Option<(usize, u32)> {
    if handle == 0 {
        return None;
    }
    let generation = (handle >> 32) as u32;
    let slot = handle as u32;
    if generation == 0 || slot == 0 {
        return None;
    }
    Some(((slot - 1) as usize, generation))
}

static SUPERVISORS: OnceLock<Mutex<Registry<Arc<SupervisorEntry>>>> = OnceLock::new();
static OUTPUTS: OnceLock<Mutex<Registry<OutputEntry>>> = OnceLock::new();

fn supervisors() -> &'static Mutex<Registry<Arc<SupervisorEntry>>> {
    SUPERVISORS.get_or_init(|| Mutex::new(Registry::new()))
}

fn outputs() -> &'static Mutex<Registry<OutputEntry>> {
    OUTPUTS.get_or_init(|| Mutex::new(Registry::new()))
}

fn registry_lock<T>(registry: &'static Mutex<T>) -> MutexGuard<'static, T> {
    registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
fn drive_after_clone_test_hook(handle: u64) {
    let barrier = DRIVE_AFTER_CLONE_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|(target, _)| *target == handle)
        .map(|(_, barrier)| barrier.clone());
    if let Some(barrier) = barrier {
        barrier.wait();
        barrier.wait();
    }
}

#[cfg(test)]
fn drive_after_arbitration_test_hook(handle: u64) {
    let barrier = DRIVE_AFTER_ARBITRATION_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|(target, _)| *target == handle)
        .map(|(_, barrier)| barrier.clone());
    if let Some(barrier) = barrier {
        barrier.wait();
        barrier.wait();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn apppilotkit_tp_v1_abi_version() -> u32 {
    catch_unwind(|| ABI_VERSION).unwrap_or(0)
}

#[unsafe(no_mangle)]
/// Creates one supervisor from a borrowed descriptor.
///
/// # Safety
/// All non-null pointers must be aligned and valid for their declared C types and lengths for the
/// duration of this call. Output pointers must not alias input storage.
pub unsafe extern "C" fn apppilotkit_tp_v1_create(
    input: *const CreateInputV1,
    out_handle: *mut u64,
    out_outcome: *mut OutcomeV1,
) -> i32 {
    ffi_entry(None, out_outcome, || {
        if !out_handle.is_null() {
            unsafe { out_handle.write(0) };
        }
        if input.is_null() || out_handle.is_null() || out_outcome.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        let header = unsafe { input.cast::<StructHeaderV1>().read() };
        if !valid_struct(
            header.abi_version,
            header.struct_size,
            std::mem::size_of::<CreateInputV1>(),
        ) {
            return STATUS_ABI_MISMATCH;
        }
        let input = unsafe { input.read() };
        let descriptor = match unsafe {
            borrowed_bytes(
                input.descriptor_cbor,
                input.descriptor_len,
                MAX_DESCRIPTOR_BYTES,
            )
        } {
            Ok(value) => value,
            Err(status) => return status,
        };
        let (transport, outcome) = match TargetTransport::create(descriptor) {
            Ok(value) => value,
            Err(error) => return supervisor_status(error),
        };
        let entry = Arc::new(SupervisorEntry::new(transport));
        let handle = match registry_lock(supervisors()).insert(entry) {
            Ok(value) => value,
            Err(()) => return STATUS_INTERNAL_PANIC,
        };
        match publish_outcome(handle, outcome, out_outcome) {
            Ok(status) => {
                unsafe { out_handle.write(handle) };
                status
            }
            Err(status) => {
                registry_lock(supervisors()).remove(handle);
                status
            }
        }
    })
}

#[unsafe(no_mangle)]
/// Drives one borrowed event through an existing supervisor.
///
/// # Safety
/// `event` and `out_outcome` must be aligned, valid, non-aliasing pointers. `event.bytes` must be
/// valid for `event.bytes_len` bytes for the duration of this synchronous call.
pub unsafe extern "C" fn apppilotkit_tp_v1_drive(
    handle: u64,
    event: *const EventV1,
    out_outcome: *mut OutcomeV1,
) -> i32 {
    ffi_entry(Some(handle), out_outcome, || {
        if event.is_null() || out_outcome.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        let header = unsafe { event.cast::<StructHeaderV1>().read() };
        if !valid_struct(
            header.abi_version,
            header.struct_size,
            std::mem::size_of::<EventV1>(),
        ) {
            return STATUS_ABI_MISMATCH;
        }
        let event = unsafe { event.read() };
        let tag = match event_tag(event.tag) {
            Some(value) => value,
            None => return STATUS_INVALID_ARGUMENT,
        };
        let byte_cap = if matches!(tag, EventTag::StreamBytes) {
            MAX_STREAM_CHUNK_BYTES
        } else {
            MAX_EVENT_BYTES
        };
        let bytes = match unsafe { borrowed_bytes(event.bytes, event.bytes_len, byte_cap) } {
            Ok(value) => value,
            Err(status) => return status,
        };
        let entry = match registry_lock(supervisors()).get(handle).cloned() {
            Some(value) => value,
            None => return STATUS_INVALID_HANDLE,
        };
        #[cfg(test)]
        drive_after_clone_test_hook(handle);
        let valid_terminal_shape =
            event.flags == 0 && event.write_token == 0 && event.bytes_len == 0;
        let mut terminal_request = None;
        if valid_terminal_shape
            && event.stream_id == 0
            && matches!(
                tag,
                EventTag::EligibilityLost | EventTag::CleanupFailed | EventTag::InternalError
            )
        {
            terminal_request = Some(match tag {
                EventTag::EligibilityLost => 1,
                EventTag::CleanupFailed => 2,
                EventTag::InternalError => 4,
                _ => unreachable!(),
            });
        }
        if valid_terminal_shape
            && matches!(
                tag,
                EventTag::StreamEof | EventTag::StreamIoFailed | EventTag::StreamCloseNormal
            )
            && event.stream_id != 0
            && entry.lease_stream.load(Ordering::Acquire) == event.stream_id
        {
            terminal_request = Some(3);
        }
        let timer_request = event.flags == 0
            && event.stream_id == 0
            && event.write_token != 0
            && event.bytes_len == 0
            && matches!(tag, EventTag::TimerFired)
            && entry.terminal_timer_is_active(event.write_token);
        if terminal_request.is_some() || timer_request {
            let _publish = entry
                .publish_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(requested) = terminal_request {
                let _ = entry.pending_terminal.compare_exchange(
                    0,
                    requested,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            if timer_request {
                let _ = entry.pending_timer.compare_exchange(
                    0,
                    event.write_token,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
        }
        if entry
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return STATUS_BUSY;
        }
        let _busy = BusyGuard(&entry.busy);
        if entry.retired.load(Ordering::Acquire) {
            return STATUS_INVALID_HANDLE;
        }
        let terminal_on_entry = entry.lock().terminal_reason();
        #[cfg(test)]
        if event.flags == u32::MAX {
            panic!("test-only FFI panic injection");
        }
        let outcome = {
            let mut transport = entry.lock();
            let result = transport.drive(Event {
                tag,
                flags: event.flags,
                stream_id: event.stream_id,
                write_token: event.write_token,
                bytes,
            });
            if matches!(&result, Ok(value) if value.kind == OutcomeKind::WriteFrames)
                && matches!(tag, EventTag::BootstrapConnected)
            {
                entry.lease_stream.store(event.stream_id, Ordering::Release);
            }
            entry.refresh_terminal_timers(&transport);
            result
        };
        let outcome = arbitrate_pending_events(&entry, tag, terminal_on_entry, outcome);
        #[cfg(test)]
        drive_after_arbitration_test_hook(handle);
        let _publish = entry
            .publish_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = arbitrate_pending_events(&entry, tag, terminal_on_entry, outcome);
        match outcome {
            Ok(value) => {
                publish_outcome(handle, value, out_outcome).unwrap_or_else(|status| status)
            }
            Err(error) => supervisor_status(error),
        }
    })
}

#[unsafe(no_mangle)]
/// Consumes one supervisor handle and writes its closed outcome.
///
/// # Safety
/// Both pointers must be aligned, writable, valid for their C types, and non-aliasing.
pub unsafe extern "C" fn apppilotkit_tp_v1_close(
    handle: *mut u64,
    out_outcome: *mut OutcomeV1,
) -> i32 {
    ffi_dynamic_entry(out_outcome, |owner| {
        if handle.is_null() || out_outcome.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        let value = unsafe { handle.read() };
        owner.set(value);
        if value == 0 {
            unsafe { out_outcome.write(outcome_v1(OutcomeKind::Closed)) };
            return STATUS_OK;
        }
        let entry = match registry_lock(supervisors()).get(value).cloned() {
            Some(entry) => entry,
            None => {
                unsafe { handle.write(0) };
                return STATUS_INVALID_HANDLE;
            }
        };
        if entry
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return STATUS_BUSY;
        }
        let _busy = BusyGuard(&entry.busy);
        entry.retired.store(true, Ordering::Release);
        let outcome = entry.lock().close();
        if registry_lock(supervisors()).remove(value).is_none() {
            unsafe { handle.write(0) };
            return STATUS_INVALID_HANDLE;
        }
        remove_owned_outputs(value);
        unsafe { handle.write(0) };
        publish_outcome(0, outcome, out_outcome).unwrap_or_else(|status| status)
    })
}

#[unsafe(no_mangle)]
/// Consumes one supervisor handle without producing an outcome.
///
/// # Safety
/// `handle` must be aligned and writable for one `u64`.
pub unsafe extern "C" fn apppilotkit_tp_v1_drop(handle: *mut u64) -> i32 {
    ffi_output_entry(|owner| {
        if handle.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        let value = unsafe { handle.read() };
        owner.set(value);
        if value == 0 {
            return STATUS_OK;
        }
        let entry = match registry_lock(supervisors()).get(value).cloned() {
            Some(entry) => entry,
            None => {
                unsafe { handle.write(0) };
                return STATUS_INVALID_HANDLE;
            }
        };
        if entry
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return STATUS_BUSY;
        }
        let _busy = BusyGuard(&entry.busy);
        entry.retired.store(true, Ordering::Release);
        let _ = entry.lock().close();
        registry_lock(supervisors()).remove(value);
        remove_owned_outputs(value);
        unsafe { handle.write(0) };
        STATUS_OK
    })
}

#[unsafe(no_mangle)]
/// Returns the number of live outputs owned by a supervisor.
///
/// # Safety
/// `out_count` must be aligned and writable for one `u64`.
pub unsafe extern "C" fn apppilotkit_tp_v1_output_count(handle: u64, out_count: *mut u64) -> i32 {
    ffi_no_outcome(Some(handle), || {
        if out_count.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        unsafe { out_count.write(0) };
        if registry_lock(supervisors()).get(handle).is_none() {
            return STATUS_INVALID_HANDLE;
        }
        let count = registry_lock(outputs())
            .slots
            .iter()
            .filter_map(|slot| slot.value.as_ref())
            .filter(|output| output.owner == handle)
            .count();
        let Ok(count) = u64::try_from(count) else {
            return STATUS_INTERNAL_PANIC;
        };
        unsafe { out_count.write(count) };
        STATUS_OK
    })
}

#[unsafe(no_mangle)]
/// Returns an output's exact byte length.
///
/// # Safety
/// `out_len` must be aligned and writable for one `u64`.
pub unsafe extern "C" fn apppilotkit_tp_v1_output_len(output: u64, out_len: *mut u64) -> i32 {
    ffi_output_entry(|owner| {
        if out_len.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        unsafe { out_len.write(0) };
        let registry = registry_lock(outputs());
        let Some(output) = registry.get(output) else {
            return STATUS_INVALID_HANDLE;
        };
        owner.set(output.owner);
        let Ok(length) = u64::try_from(output.bytes.0.len()) else {
            return STATUS_INTERNAL_PANIC;
        };
        unsafe { out_len.write(length) };
        STATUS_OK
    })
}

#[unsafe(no_mangle)]
/// Copies an output into caller-owned storage without consuming the output handle.
///
/// # Safety
/// `out_written` must be aligned and writable for one `u64`; when the output is non-empty,
/// `destination` must be valid for `capacity` writable bytes and must not overlap Rust storage.
pub unsafe extern "C" fn apppilotkit_tp_v1_output_copy(
    output: u64,
    destination: *mut u8,
    capacity: u64,
    out_written: *mut u64,
) -> i32 {
    ffi_output_entry(|owner| {
        if out_written.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        unsafe { out_written.write(0) };
        let registry = registry_lock(outputs());
        let Some(output) = registry.get(output) else {
            return STATUS_INVALID_HANDLE;
        };
        owner.set(output.owner);
        let required = output.bytes.0.len();
        let Ok(required_u64) = u64::try_from(required) else {
            return STATUS_INTERNAL_PANIC;
        };
        if capacity < required_u64 {
            unsafe { out_written.write(required_u64) };
            return STATUS_BUFFER_TOO_SMALL;
        }
        if required != 0 && destination.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        let Ok(capacity) = usize::try_from(capacity) else {
            return STATUS_INVALID_ARGUMENT;
        };
        if capacity < required {
            return STATUS_BUFFER_TOO_SMALL;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(output.bytes.0.as_ptr(), destination, required);
            out_written.write(required_u64);
        }
        STATUS_OK
    })
}

#[unsafe(no_mangle)]
/// Consumes and zeroizes one output handle.
///
/// # Safety
/// `output` must be aligned and writable for one `u64`.
pub unsafe extern "C" fn apppilotkit_tp_v1_output_drop(output: *mut u64) -> i32 {
    ffi_output_entry(|owner| {
        if output.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        let value = unsafe { output.read() };
        if value == 0 {
            return STATUS_OK;
        }
        let mut registry = registry_lock(outputs());
        if let Some(output) = registry.get(value) {
            owner.set(output.owner);
        }
        let removed = registry.remove(value);
        unsafe { output.write(0) };
        if removed.is_some() {
            STATUS_OK
        } else {
            STATUS_INVALID_HANDLE
        }
    })
}

fn ffi_entry(owner: Option<u64>, out_outcome: *mut OutcomeV1, body: impl FnOnce() -> i32) -> i32 {
    if !out_outcome.is_null() {
        unsafe { out_outcome.write(OutcomeV1::zeroed()) };
    }
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(_) => {
            if let Some(owner) = owner
                && let Some(entry) = registry_lock(supervisors()).get(owner).cloned()
            {
                entry.terminate_internal();
            }
            STATUS_INTERNAL_PANIC
        }
    }
}

fn ffi_dynamic_entry(
    out_outcome: *mut OutcomeV1,
    body: impl FnOnce(&std::cell::Cell<u64>) -> i32,
) -> i32 {
    if !out_outcome.is_null() {
        unsafe { out_outcome.write(OutcomeV1::zeroed()) };
    }
    let owner = std::cell::Cell::new(0);
    match catch_unwind(AssertUnwindSafe(|| body(&owner))) {
        Ok(status) => status,
        Err(_) => {
            let owner = owner.get();
            if owner != 0
                && let Some(entry) = registry_lock(supervisors()).get(owner).cloned()
            {
                entry.terminate_internal();
            }
            STATUS_INTERNAL_PANIC
        }
    }
}

fn ffi_no_outcome(owner: Option<u64>, body: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(_) => {
            if let Some(owner) = owner
                && let Some(entry) = registry_lock(supervisors()).get(owner).cloned()
            {
                entry.terminate_internal();
            }
            STATUS_INTERNAL_PANIC
        }
    }
}

fn ffi_output_entry(body: impl FnOnce(&std::cell::Cell<u64>) -> i32) -> i32 {
    let owner = std::cell::Cell::new(0);
    match catch_unwind(AssertUnwindSafe(|| body(&owner))) {
        Ok(status) => status,
        Err(_) => {
            let owner = owner.get();
            if owner != 0
                && let Some(entry) = registry_lock(supervisors()).get(owner).cloned()
            {
                entry.terminate_internal();
            }
            STATUS_INTERNAL_PANIC
        }
    }
}

fn publish_outcome(
    owner: u64,
    mut outcome: Outcome,
    destination: *mut OutcomeV1,
) -> Result<i32, i32> {
    if destination.is_null() {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    let output = if let Some(bytes) = outcome.bytes.take() {
        registry_lock(outputs())
            .insert(OutputEntry {
                owner,
                bytes: OwnedOutput(bytes),
            })
            .map_err(|()| STATUS_INTERNAL_PANIC)?
    } else {
        0
    };
    let mut ffi = OutcomeV1::zeroed();
    ffi.abi_version = ABI_VERSION;
    ffi.struct_size = std::mem::size_of::<OutcomeV1>() as u32;
    ffi.kind = outcome_kind(outcome.kind);
    ffi.flags = outcome.flags;
    ffi.stream_id = outcome.stream_id;
    ffi.write_token = outcome.write_token;
    ffi.output = output;
    ffi.value0 = outcome.value0;
    ffi.value1 = outcome.value1;
    ffi.next_deadline_ms = outcome.next_deadline_ms;
    ffi.close_reason = outcome.close_reason as u32;
    ffi.handoff_state = outcome.handoff as u32;
    if let Some((reason, handoff)) = outcome.peer_close {
        ffi.flags |= 1;
        ffi.peer_close_reason = reason as u32;
        ffi.peer_handoff_state = handoff as u32;
    }
    unsafe { destination.write(ffi) };
    Ok(status_for_outcome(outcome.kind))
}

fn remove_owned_outputs(owner: u64) {
    let mut registry = registry_lock(outputs());
    for slot in &mut registry.slots {
        if slot
            .value
            .as_ref()
            .is_some_and(|output| output.owner == owner)
        {
            slot.value = None;
            slot.generation = slot.generation.wrapping_add(1).max(1);
        }
    }
}

unsafe fn borrowed_bytes<'a>(pointer: *const u8, length: u64, cap: u64) -> Result<&'a [u8], i32> {
    if length > cap {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    let length = usize::try_from(length).map_err(|_| STATUS_INVALID_ARGUMENT)?;
    if length == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    Ok(unsafe { std::slice::from_raw_parts(pointer, length) })
}

fn valid_struct(version: u32, size: u32, expected: usize) -> bool {
    version == ABI_VERSION && usize::try_from(size).is_ok_and(|size| size >= expected)
}

fn event_tag(tag: u32) -> Option<EventTag> {
    Some(match tag {
        EVENT_BOOTSTRAP_CONNECTED => EventTag::BootstrapConnected,
        EVENT_STREAM_BYTES => EventTag::StreamBytes,
        EVENT_FULL_WRITE_COMMITTED => EventTag::FullWriteCommitted,
        EVENT_SESSION_ACCEPTED => EventTag::SessionAccepted,
        EVENT_RUNTIME_RESPONSE => EventTag::RuntimeResponse,
        EVENT_STREAM_EOF => EventTag::StreamEof,
        EVENT_STREAM_IO_FAILED => EventTag::StreamIoFailed,
        EVENT_STREAM_CLOSE_NORMAL => EventTag::StreamCloseNormal,
        EVENT_TIMER_FIRED => EventTag::TimerFired,
        EVENT_ELIGIBILITY_LOST => EventTag::EligibilityLost,
        EVENT_CLEANUP_FAILED => EventTag::CleanupFailed,
        EVENT_INTERNAL_ERROR => EventTag::InternalError,
        _ => return None,
    })
}

const fn outcome_kind(kind: OutcomeKind) -> u32 {
    match kind {
        OutcomeKind::EndpointReady => OUTCOME_ENDPOINT_READY,
        OutcomeKind::WriteFrames => OUTCOME_WRITE_FRAMES,
        OutcomeKind::Application => OUTCOME_APPLICATION,
        OutcomeKind::LeaseReady => OUTCOME_LEASE_READY,
        OutcomeKind::NeedInput => OUTCOME_NEED_INPUT,
        OutcomeKind::SessionTerminal => OUTCOME_SESSION_TERMINAL,
        OutcomeKind::LeaseTerminal => OUTCOME_LEASE_TERMINAL,
        OutcomeKind::Closed => OUTCOME_CLOSED,
    }
}

const fn status_for_outcome(kind: OutcomeKind) -> i32 {
    match kind {
        OutcomeKind::NeedInput => STATUS_NEED_INPUT,
        OutcomeKind::SessionTerminal | OutcomeKind::LeaseTerminal => STATUS_TERMINAL,
        OutcomeKind::Closed => STATUS_OK,
        _ => STATUS_EVENT,
    }
}

const fn supervisor_status(error: SupervisorError) -> i32 {
    match error {
        SupervisorError::InvalidArgument => STATUS_INVALID_ARGUMENT,
        SupervisorError::WrongPhase => STATUS_WRONG_PHASE,
        SupervisorError::Internal => STATUS_INTERNAL_PANIC,
    }
}

fn outcome_v1(kind: OutcomeKind) -> OutcomeV1 {
    let mut outcome = OutcomeV1::zeroed();
    outcome.abi_version = ABI_VERSION;
    outcome.struct_size = std::mem::size_of::<OutcomeV1>() as u32;
    outcome.kind = outcome_kind(kind);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use apppilotkit_transport_crypto_core::{
        BootstrapBinding, BrokerBootstrap, BrokerSession, BrokerStaticKeypair,
        BrokerStaticPrivateKey, ProcessBootstrapSecret, SessionBinding,
    };
    use minicbor::Encoder;

    const APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_VALUE0: u32 = 1 << 1;
    const APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_WRITE_TOKEN: u32 = 1 << 2;
    use sha2::{Digest, Sha256};
    use std::sync::Barrier;
    use std::thread;

    fn descriptor() -> Vec<u8> {
        let binding = BootstrapBinding {
            target_reference_digest: [0x41; 32],
            lease_id: [0x51; 16],
            target_nonce: [0x61; 32],
            app_artifact_digest: [0x71; 32],
            expiry_ms: 1_893_456_000_000,
        };
        let key = BrokerStaticKeypair::generate()
            .expect("keypair")
            .public_key();
        let mut bytes = Vec::new();
        Encoder::new(&mut bytes)
            .map(9)
            .unwrap()
            .u8(0)
            .unwrap()
            .u8(1)
            .unwrap()
            .u8(1)
            .unwrap()
            .u8(0)
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
            .bytes(&key)
            .unwrap()
            .u8(6)
            .unwrap()
            .map(2)
            .unwrap()
            .u8(0)
            .unwrap()
            .str("127.0.0.1")
            .unwrap()
            .u8(1)
            .unwrap()
            .u16(55_001)
            .unwrap()
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

    fn descriptor_and_broker() -> (
        Vec<u8>,
        BootstrapBinding,
        BrokerStaticPrivateKey,
        ProcessBootstrapSecret,
    ) {
        let vector = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/v1/vectors/bootstrap-nk-success.json"
        ));
        let binding = BootstrapBinding {
            target_reference_digest: vector_hex(vector, "target_reference_digest_hex")
                .try_into()
                .expect("32-byte D0 target reference digest"),
            lease_id: [0x51; 16],
            target_nonce: [0x71; 32],
            app_artifact_digest: [0x81; 32],
            expiry_ms: 1_893_456_000_000,
        };
        let bytes = vector_hex(vector, "launch_descriptor_cbor_hex");
        let private = BrokerStaticPrivateKey::new(
            vector_hex(vector, "broker_static_private_hex")
                .try_into()
                .expect("32-byte D0 static private key"),
        );
        let pbs = ProcessBootstrapSecret::new(
            vector_hex(vector, "process_bootstrap_secret_hex")
                .try_into()
                .expect("32-byte D0 PBS"),
        );
        (bytes, binding, private, pbs)
    }

    fn vector_hex(vector: &str, key: &str) -> Vec<u8> {
        let marker = format!("\"{key}\": \"");
        let start = vector.find(&marker).expect("D0 vector key") + marker.len();
        let end = vector[start..].find('"').expect("D0 vector hex terminator") + start;
        let hex = &vector[start..end];
        assert_eq!(hex.len() % 2, 0);
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("D0 hex"))
            .collect()
    }

    fn create() -> (u64, OutcomeV1) {
        let bytes = descriptor();
        let input = CreateInputV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<CreateInputV1>() as u32,
            descriptor_cbor: bytes.as_ptr(),
            descriptor_len: bytes.len() as u64,
        };
        let mut handle = 0;
        let mut outcome = OutcomeV1::zeroed();
        let status = unsafe { apppilotkit_tp_v1_create(&input, &mut handle, &mut outcome) };
        assert_eq!(status, STATUS_EVENT);
        assert_ne!(handle, 0);
        assert_eq!(outcome.kind, OUTCOME_ENDPOINT_READY);
        assert_eq!(outcome.value0, 0);
        assert_eq!(outcome.value1, 55_001);
        assert_eq!(outcome.output, 0);
        (handle, outcome)
    }

    fn create_with_descriptor(bytes: &[u8]) -> (u64, OutcomeV1) {
        let input = CreateInputV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<CreateInputV1>() as u32,
            descriptor_cbor: bytes.as_ptr(),
            descriptor_len: bytes.len() as u64,
        };
        let mut handle = 0;
        let mut outcome = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_create(&input, &mut handle, &mut outcome) },
            STATUS_EVENT
        );
        (handle, outcome)
    }

    fn vector_string<'a>(vector: &'a str, key: &str) -> &'a str {
        let marker = format!("\"{key}\": \"");
        let start = vector.find(&marker).expect("D0 vector key") + marker.len();
        let end = vector[start..].find('"').expect("D0 string terminator") + start;
        &vector[start..end]
    }

    fn drive_ffi(
        handle: u64,
        tag: u32,
        stream_id: u64,
        write_token: u64,
        bytes: &[u8],
    ) -> (i32, OutcomeV1, Vec<u8>) {
        let event = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag,
            flags: 0,
            stream_id,
            write_token,
            bytes: if bytes.is_empty() {
                std::ptr::null()
            } else {
                bytes.as_ptr()
            },
            bytes_len: bytes.len() as u64,
        };
        let mut outcome = OutcomeV1::zeroed();
        let status = unsafe { apppilotkit_tp_v1_drive(handle, &event, &mut outcome) };
        let copied = if outcome.output == 0 {
            Vec::new()
        } else {
            let mut len = 0;
            assert_eq!(
                unsafe { apppilotkit_tp_v1_output_len(outcome.output, &mut len) },
                STATUS_OK
            );
            let mut copied = vec![0; len as usize];
            let mut written = 0;
            assert_eq!(
                unsafe {
                    apppilotkit_tp_v1_output_copy(
                        outcome.output,
                        copied.as_mut_ptr(),
                        len,
                        &mut written,
                    )
                },
                STATUS_OK
            );
            assert_eq!(written, len);
            let mut output = outcome.output;
            assert_eq!(
                unsafe { apppilotkit_tp_v1_output_drop(&mut output) },
                STATUS_OK
            );
            copied
        };
        (status, outcome, copied)
    }

    #[test]
    fn handles_and_outputs_are_generation_safe_and_consuming_drops_are_idempotent() {
        let zeroized_before = ZEROIZED_OUTPUT_DROPS.load(Ordering::Relaxed);
        let (mut handle, _) = create();
        let stale = handle;
        let event = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_BOOTSTRAP_CONNECTED,
            flags: 0,
            stream_id: 9,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let mut outcome = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &event, &mut outcome) },
            STATUS_EVENT
        );
        assert_ne!(outcome.output, 0);
        let mut length = 0;
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_len(outcome.output, &mut length) },
            STATUS_OK
        );
        let mut small = [0_u8; 1];
        let mut written = 99;
        assert_eq!(
            unsafe {
                apppilotkit_tp_v1_output_copy(
                    outcome.output,
                    small.as_mut_ptr(),
                    small.len() as u64,
                    &mut written,
                )
            },
            STATUS_BUFFER_TOO_SMALL
        );
        assert_eq!(written, length);
        let mut bytes = vec![0; length as usize];
        assert_eq!(
            unsafe {
                apppilotkit_tp_v1_output_copy(
                    outcome.output,
                    bytes.as_mut_ptr(),
                    bytes.len() as u64,
                    &mut written,
                )
            },
            STATUS_OK
        );
        let mut output = outcome.output;
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_drop(&mut output) },
            STATUS_OK
        );
        assert!(ZEROIZED_OUTPUT_DROPS.load(Ordering::Relaxed) > zeroized_before);
        assert_eq!(output, 0);
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_drop(&mut output) },
            STATUS_OK
        );
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
        assert_eq!(handle, 0);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(stale, &event, &mut outcome) },
            STATUS_INVALID_HANDLE
        );
    }

    #[test]
    fn null_overflow_version_and_panic_inputs_fail_with_zero_outcomes() {
        let (mut handle, _) = create();
        let mut outcome = OutcomeV1 {
            reserved: [u64::MAX; 4],
            ..OutcomeV1::zeroed()
        };
        let bad = EventV1 {
            abi_version: 0,
            struct_size: 0,
            tag: EVENT_STREAM_BYTES,
            flags: 0,
            stream_id: 1,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: u64::MAX,
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &bad, &mut outcome) },
            STATUS_ABI_MISMATCH
        );
        assert_eq!(outcome.kind, 0);
        assert_eq!(outcome.reserved, [0; 4]);

        let panic_event = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_STREAM_BYTES,
            flags: u32::MAX,
            stream_id: 1,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &panic_event, &mut outcome) },
            STATUS_INTERNAL_PANIC
        );
        assert_eq!(outcome.kind, 0);
        let terminal = EventV1 {
            flags: 0,
            tag: EVENT_BOOTSTRAP_CONNECTED,
            stream_id: 1,
            ..panic_event
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &terminal, &mut outcome) },
            STATUS_TERMINAL
        );
        assert_eq!(outcome.kind, OUTCOME_LEASE_TERMINAL);
        assert_eq!(outcome.close_reason, 13);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn android_create_returns_validated_endpoint_through_consuming_output_registry() {
        let vector = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/v1/vectors/bootstrap-android-descriptor.json"
        ));
        let descriptor = vector_hex(vector, "launch_descriptor_cbor_hex");
        let expected = vector_string(vector, "localabstract_name").as_bytes();
        assert!((32..=96).contains(&expected.len()));
        assert!(!expected.contains(&0));

        let (mut handle, outcome) = create_with_descriptor(&descriptor);
        assert_eq!(outcome.kind, OUTCOME_ENDPOINT_READY);
        assert_eq!(outcome.value0, 1);
        assert_eq!(outcome.value1, 0);
        assert_ne!(outcome.output, 0);
        let mut count = 0;
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_count(handle, &mut count) },
            STATUS_OK
        );
        assert_eq!(count, 1);
        let mut length = 0;
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_len(outcome.output, &mut length) },
            STATUS_OK
        );
        assert_eq!(length as usize, expected.len());
        let mut copied = vec![0xA5; expected.len() + 1];
        let mut written = 0;
        assert_eq!(
            unsafe {
                apppilotkit_tp_v1_output_copy(
                    outcome.output,
                    copied.as_mut_ptr(),
                    copied.len() as u64,
                    &mut written,
                )
            },
            STATUS_OK
        );
        assert_eq!(written as usize, expected.len());
        assert_eq!(&copied[..expected.len()], expected);
        assert_eq!(copied[expected.len()], 0xA5, "endpoint has no NUL suffix");

        let stale = outcome.output;
        let mut output = outcome.output;
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_drop(&mut output) },
            STATUS_OK
        );
        assert_eq!(output, 0);
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_count(handle, &mut count) },
            STATUS_OK
        );
        assert_eq!(count, 0);
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_len(stale, &mut length) },
            STATUS_INVALID_HANDLE
        );
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);

        let zeroized_before = ZEROIZED_OUTPUT_DROPS.load(Ordering::Relaxed);
        let (mut handle, outcome) = create_with_descriptor(&descriptor);
        let stale = outcome.output;
        let mut closed = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_close(&mut handle, &mut closed) },
            STATUS_OK
        );
        assert_eq!(handle, 0);
        assert!(ZEROIZED_OUTPUT_DROPS.load(Ordering::Relaxed) > zeroized_before);
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_len(stale, &mut length) },
            STATUS_INVALID_HANDLE
        );

        let mut invalid = descriptor;
        invalid.push(0);
        let input = CreateInputV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<CreateInputV1>() as u32,
            descriptor_cbor: invalid.as_ptr(),
            descriptor_len: invalid.len() as u64,
        };
        let mut handle = u64::MAX;
        let mut rejected = outcome_v1(OutcomeKind::Closed);
        assert_eq!(
            unsafe { apppilotkit_tp_v1_create(&input, &mut handle, &mut rejected) },
            STATUS_INVALID_ARGUMENT
        );
        assert_eq!(handle, 0);
        assert_eq!(rejected.output, 0);
        assert_eq!(rejected.kind, 0);
    }

    #[test]
    fn registry_allows_different_supervisors_to_be_owned_concurrently() {
        let (mut first, _) = create();
        let (mut second, _) = create();
        let barrier = Arc::new(Barrier::new(3));
        let entries = [first, second].map(|handle| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                let entry = registry_lock(supervisors())
                    .get(handle)
                    .cloned()
                    .expect("entry");
                assert!(
                    entry
                        .busy
                        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                );
                let _guard = BusyGuard(&entry.busy);
                barrier.wait();
                barrier.wait();
            })
        });
        barrier.wait();
        assert!(
            registry_lock(supervisors())
                .get(first)
                .unwrap()
                .busy
                .load(Ordering::Acquire)
        );
        assert!(
            registry_lock(supervisors())
                .get(second)
                .unwrap()
                .busy
                .load(Ordering::Acquire)
        );
        barrier.wait();
        for thread in entries {
            thread.join().unwrap();
        }
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut first) }, STATUS_OK);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut second) }, STATUS_OK);
    }

    #[test]
    fn close_retires_and_terminalizes_an_entry_cloned_by_a_paused_drive() {
        let _serial = CONCURRENCY_HOOK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut handle, _) = create();
        let (_, m1, _) = drive_ffi(handle, EVENT_BOOTSTRAP_CONNECTED, 7, 0, &[]);
        let m1_token = m1.write_token;
        let held_entry = registry_lock(supervisors())
            .get(handle)
            .cloned()
            .expect("entry");
        let stale = handle;
        let barrier = Arc::new(Barrier::new(2));
        *DRIVE_AFTER_CLONE_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((handle, barrier.clone()));

        let worker = thread::spawn(move || {
            let event = EventV1 {
                abi_version: ABI_VERSION,
                struct_size: std::mem::size_of::<EventV1>() as u32,
                tag: EVENT_FULL_WRITE_COMMITTED,
                flags: 0,
                stream_id: 7,
                write_token: m1_token,
                bytes: std::ptr::null(),
                bytes_len: 0,
            };
            let mut outcome = OutcomeV1::zeroed();
            let status = unsafe { apppilotkit_tp_v1_drive(stale, &event, &mut outcome) };
            (status, outcome)
        });

        // Wait until drive has cloned the entry, then let close consume and retire it.
        let hook_barrier = DRIVE_AFTER_CLONE_HOOK
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .unwrap()
            .1
            .clone();
        hook_barrier.wait();
        let mut closed = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_close(&mut handle, &mut closed) },
            STATUS_OK
        );
        assert_eq!(handle, 0);
        let terminal = held_entry
            .lock()
            .drive(Event {
                tag: EventTag::FullWriteCommitted,
                flags: 0,
                stream_id: 7,
                write_token: m1_token,
                bytes: &[],
            })
            .expect("retired transport terminal");
        assert_eq!(terminal.kind, OutcomeKind::LeaseTerminal);
        hook_barrier.wait();
        let (status, outcome) = worker.join().unwrap();
        assert_eq!(status, STATUS_INVALID_HANDLE);
        assert_eq!(outcome.kind, 0);
        *DRIVE_AFTER_CLONE_HOOK
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    #[test]
    fn drop_terminalizes_secret_state_before_a_paused_clone_is_released() {
        let _serial = CONCURRENCY_HOOK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut handle, _) = create();
        let (_, m1, _) = drive_ffi(handle, EVENT_BOOTSTRAP_CONNECTED, 17, 0, &[]);
        let m1_token = m1.write_token;
        let held_entry = registry_lock(supervisors())
            .get(handle)
            .cloned()
            .expect("entry");
        let stale = handle;
        let barrier = Arc::new(Barrier::new(2));
        *DRIVE_AFTER_CLONE_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((handle, barrier.clone()));
        let worker = thread::spawn(move || {
            let event = EventV1 {
                abi_version: ABI_VERSION,
                struct_size: std::mem::size_of::<EventV1>() as u32,
                tag: EVENT_FULL_WRITE_COMMITTED,
                flags: 0,
                stream_id: 17,
                write_token: m1_token,
                bytes: std::ptr::null(),
                bytes_len: 0,
            };
            let mut outcome = OutcomeV1::zeroed();
            unsafe { apppilotkit_tp_v1_drive(stale, &event, &mut outcome) }
        });
        barrier.wait();
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
        assert_eq!(handle, 0);
        let terminal = held_entry
            .lock()
            .drive(Event {
                tag: EventTag::FullWriteCommitted,
                flags: 0,
                stream_id: 17,
                write_token: m1_token,
                bytes: &[],
            })
            .expect("dropped transport terminal");
        assert_eq!(terminal.kind, OutcomeKind::LeaseTerminal);
        barrier.wait();
        assert_eq!(worker.join().unwrap(), STATUS_INVALID_HANDLE);
        *DRIVE_AFTER_CLONE_HOOK
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    #[test]
    fn production_abi_harness_uses_d0_literal_and_direct_core_broker() {
        let (descriptor, bootstrap_binding, broker_private, broker_pbs) = descriptor_and_broker();
        let mut noncanonical = descriptor.clone();
        noncanonical.push(0);
        let input = CreateInputV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<CreateInputV1>() as u32,
            descriptor_cbor: noncanonical.as_ptr(),
            descriptor_len: noncanonical.len() as u64,
        };
        let mut rejected_handle = u64::MAX;
        let mut rejected_outcome = outcome_v1(OutcomeKind::Closed);
        assert_eq!(
            unsafe {
                apppilotkit_tp_v1_create(&input, &mut rejected_handle, &mut rejected_outcome)
            },
            STATUS_INVALID_ARGUMENT
        );
        assert_eq!(rejected_handle, 0);
        assert_eq!(rejected_outcome.kind, 0);

        let broker_bootstrap = BrokerBootstrap::new(bootstrap_binding, broker_private, &broker_pbs)
            .expect("Broker bootstrap");
        let (mut handle, _) = create_with_descriptor(&descriptor);
        let (status, m1, m1_bytes) = drive_ffi(handle, EVENT_BOOTSTRAP_CONNECTED, 7, 0, &[]);
        assert_eq!(status, STATUS_EVENT);
        assert_eq!(m1.kind, OUTCOME_WRITE_FRAMES);
        let (m2, broker_ack) = broker_bootstrap
            .read_m1_write_m2(&m1_bytes)
            .expect("Broker M2");
        assert_eq!(
            drive_ffi(handle, EVENT_FULL_WRITE_COMMITTED, 7, m1.write_token, &[]).0,
            STATUS_NEED_INPUT
        );
        let (_, ack, ack_bytes) = drive_ffi(handle, EVENT_STREAM_BYTES, 7, 0, &m2);
        let (verified, _lease) = broker_ack.read_ack(&ack_bytes).expect("ACK");
        let (status, ready, _) =
            drive_ffi(handle, EVENT_FULL_WRITE_COMMITTED, 7, ack.write_token, &[]);
        assert_eq!(status, STATUS_EVENT);
        assert_eq!(ready.kind, OUTCOME_LEASE_READY);
        assert_eq!(ready.value0, verified.process_generation);
        assert_ne!(
            ready.write_token, 0,
            "lease tick deadline token is explicit"
        );
        assert_ne!(
            ready.flags & APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_WRITE_TOKEN,
            0,
            "APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_WRITE_TOKEN is set"
        );
        assert_eq!(
            ready.flags & APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_VALUE0,
            0,
            "lease generation remains in value0 rather than holding the deadline token"
        );
        assert_ne!(ready.write_token, ready.value0);
        let (tick_status, next_tick, _) =
            drive_ffi(handle, EVENT_TIMER_FIRED, 0, ready.write_token, &[]);
        assert_eq!(tick_status, STATUS_NEED_INPUT);
        assert_eq!(next_tick.kind, OUTCOME_NEED_INPUT);
        assert_ne!(
            next_tick.flags & APK_TP_OUTCOME_FLAG_DEADLINE_TOKEN_WRITE_TOKEN,
            0
        );

        let (_, session_m1, session_m1_bytes) =
            drive_ffi(handle, EVENT_SESSION_ACCEPTED, 11, 0, &[]);
        let binding = SessionBinding {
            lease_id: verified.lease_id,
            process_generation: verified.process_generation,
            listener_epoch: verified.listener_epoch,
            nk_handshake_hash: verified.nk_handshake_hash,
        };
        let mut broker = BrokerSession::new(binding, &broker_pbs).expect("Broker session");
        let session_m2 = broker
            .read_m1_write_m2(&session_m1_bytes)
            .expect("session M2");
        drive_ffi(
            handle,
            EVENT_FULL_WRITE_COMMITTED,
            11,
            session_m1.write_token,
            &[],
        );
        let (_, target_finished, target_finished_bytes) =
            drive_ffi(handle, EVENT_STREAM_BYTES, 11, 0, &session_m2);
        broker
            .read_finished(&target_finished_bytes)
            .expect("Target Finished");
        drive_ffi(
            handle,
            EVENT_FULL_WRITE_COMMITTED,
            11,
            target_finished.write_token,
            &[],
        );
        let broker_finished = broker.write_finished().expect("Broker Finished");
        assert_eq!(
            drive_ffi(handle, EVENT_STREAM_BYTES, 11, 0, &broker_finished).0,
            STATUS_NEED_INPUT
        );

        let open_frames = broker
            .write_session_open(b"opaque session.open")
            .expect("open");
        let open_bytes = open_frames.into_iter().flatten().collect::<Vec<_>>();
        let (_, application, application_bytes) =
            drive_ffi(handle, EVENT_STREAM_BYTES, 11, 0, &open_bytes);
        assert_eq!(application.kind, OUTCOME_APPLICATION);
        assert_eq!(application_bytes, b"opaque session.open");
        let (_, response, response_bytes) =
            drive_ffi(handle, EVENT_RUNTIME_RESPONSE, 11, 0, b"opaque response");
        let mut decoder = apppilotkit_transport_crypto_core::OuterFrameDecoder::new();
        let frames = decoder
            .push(&response_bytes)
            .expect("response outer frames");
        assert_eq!(frames.len(), 1);
        assert_eq!(
            broker
                .read_application_response(&frames[0])
                .expect("response"),
            Some(b"opaque response".to_vec())
        );
        drive_ffi(
            handle,
            EVENT_FULL_WRITE_COMMITTED,
            11,
            response.write_token,
            &[],
        );
        let mut closed = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_close(&mut handle, &mut closed) },
            STATUS_OK
        );
        assert_eq!(handle, 0);
        assert_eq!(closed.kind, OUTCOME_CLOSED);
    }

    #[test]
    fn busy_terminal_request_is_latched_and_wins_before_next_publish() {
        let (mut handle, _) = create();
        let entry = registry_lock(supervisors())
            .get(handle)
            .cloned()
            .expect("entry");
        entry.busy.store(true, Ordering::Release);
        let eligibility = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_ELIGIBILITY_LOST,
            flags: 0,
            stream_id: 0,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let mut outcome = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &eligibility, &mut outcome) },
            STATUS_BUSY
        );
        entry.busy.store(false, Ordering::Release);
        let bootstrap = EventV1 {
            tag: EVENT_BOOTSTRAP_CONNECTED,
            stream_id: 9,
            ..eligibility
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &bootstrap, &mut outcome) },
            STATUS_TERMINAL
        );
        assert_eq!(outcome.kind, OUTCOME_LEASE_TERMINAL);
        assert_eq!(outcome.close_reason, 11);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn internal_error_event_requires_zero_shape_and_terminalizes_the_lease() {
        let (mut handle, _) = create();
        let entry = registry_lock(supervisors())
            .get(handle)
            .cloned()
            .expect("entry");
        let valid = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_INTERNAL_ERROR,
            flags: 0,
            stream_id: 0,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let byte = 1_u8;
        let mut outcome = OutcomeV1::zeroed();
        entry.busy.store(true, Ordering::Release);
        let invalid_while_busy = EventV1 {
            stream_id: 1,
            ..valid
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &invalid_while_busy, &mut outcome) },
            STATUS_BUSY
        );
        assert_eq!(entry.pending_terminal.load(Ordering::Acquire), 0);
        entry.busy.store(false, Ordering::Release);
        for invalid in [
            EventV1 {
                stream_id: 1,
                ..valid
            },
            EventV1 {
                write_token: 1,
                ..valid
            },
            EventV1 { flags: 1, ..valid },
            EventV1 {
                bytes: &byte,
                bytes_len: 1,
                ..valid
            },
        ] {
            assert_eq!(
                unsafe { apppilotkit_tp_v1_drive(handle, &invalid, &mut outcome) },
                STATUS_INVALID_ARGUMENT
            );
            assert_eq!(outcome.kind, 0, "invalid input must leave a zero outcome");
        }
        let bootstrap = EventV1 {
            tag: EVENT_BOOTSTRAP_CONNECTED,
            stream_id: 9,
            ..valid
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &bootstrap, &mut outcome) },
            STATUS_EVENT,
            "invalid InternalError events must not terminalize the lease"
        );
        assert_eq!(outcome.kind, OUTCOME_WRITE_FRAMES);
        let mut output = outcome.output;
        assert_ne!(output, 0);
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_drop(&mut output) },
            STATUS_OK
        );
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &valid, &mut outcome) },
            STATUS_TERMINAL
        );
        assert_eq!(outcome.kind, OUTCOME_LEASE_TERMINAL);
        assert_eq!(outcome.close_reason, 13);
        assert_eq!(outcome.output, 0);
        assert_eq!(outcome.peer_close_reason, 0);
        assert_eq!(outcome.peer_handoff_state, 0);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn busy_internal_error_is_latched_and_wins_before_next_publish() {
        let (mut handle, _) = create();
        let entry = registry_lock(supervisors())
            .get(handle)
            .cloned()
            .expect("entry");
        entry.busy.store(true, Ordering::Release);
        let internal = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_INTERNAL_ERROR,
            flags: 0,
            stream_id: 0,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let mut outcome = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &internal, &mut outcome) },
            STATUS_BUSY
        );
        entry.busy.store(false, Ordering::Release);
        let bootstrap = EventV1 {
            tag: EVENT_BOOTSTRAP_CONNECTED,
            stream_id: 9,
            ..internal
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &bootstrap, &mut outcome) },
            STATUS_TERMINAL
        );
        assert_eq!(outcome.kind, OUTCOME_LEASE_TERMINAL);
        assert_eq!(outcome.close_reason, 13);
        assert_eq!(outcome.output, 0);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn busy_internal_error_wins_over_later_terminal_events() {
        for later_tag in [EVENT_ELIGIBILITY_LOST, EVENT_CLEANUP_FAILED] {
            let (mut handle, _) = create();
            let entry = registry_lock(supervisors())
                .get(handle)
                .cloned()
                .expect("entry");
            entry.busy.store(true, Ordering::Release);
            let internal = EventV1 {
                abi_version: ABI_VERSION,
                struct_size: std::mem::size_of::<EventV1>() as u32,
                tag: EVENT_INTERNAL_ERROR,
                flags: 0,
                stream_id: 0,
                write_token: 0,
                bytes: std::ptr::null(),
                bytes_len: 0,
            };
            let mut outcome = OutcomeV1::zeroed();
            assert_eq!(
                unsafe { apppilotkit_tp_v1_drive(handle, &internal, &mut outcome) },
                STATUS_BUSY
            );
            entry.busy.store(false, Ordering::Release);
            let later = EventV1 {
                tag: later_tag,
                ..internal
            };
            assert_eq!(
                unsafe { apppilotkit_tp_v1_drive(handle, &later, &mut outcome) },
                STATUS_TERMINAL
            );
            assert_eq!(outcome.kind, OUTCOME_LEASE_TERMINAL);
            assert_eq!(outcome.close_reason, 13);
            assert_eq!(outcome.output, 0);
            assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
        }
    }

    #[test]
    fn terminal_latched_after_first_arbitration_wins_before_publish() {
        let _serial = CONCURRENCY_HOOK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut handle, _) = create();
        let stale = handle;
        let barrier = Arc::new(Barrier::new(2));
        *DRIVE_AFTER_ARBITRATION_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((handle, barrier.clone()));
        let worker = thread::spawn(move || {
            let event = EventV1 {
                abi_version: ABI_VERSION,
                struct_size: std::mem::size_of::<EventV1>() as u32,
                tag: EVENT_BOOTSTRAP_CONNECTED,
                flags: 0,
                stream_id: 23,
                write_token: 0,
                bytes: std::ptr::null(),
                bytes_len: 0,
            };
            let mut outcome = OutcomeV1::zeroed();
            let status = unsafe { apppilotkit_tp_v1_drive(stale, &event, &mut outcome) };
            (status, outcome)
        });
        barrier.wait();
        let terminal = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_INTERNAL_ERROR,
            flags: 0,
            stream_id: 0,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let mut ignored = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &terminal, &mut ignored) },
            STATUS_BUSY
        );
        barrier.wait();
        let (status, outcome) = worker.join().unwrap();
        assert_eq!(status, STATUS_TERMINAL);
        assert_eq!(outcome.kind, OUTCOME_LEASE_TERMINAL);
        assert_eq!(outcome.close_reason, 13);
        assert_eq!(outcome.output, 0);
        *DRIVE_AFTER_ARBITRATION_HOOK
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn published_terminal_reason_survives_late_busy_internal_error() {
        let _serial = CONCURRENCY_HOOK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for initial_reason in [4, 11] {
            let (mut handle, _) = create();
            let (status, bootstrap, _) = drive_ffi(handle, EVENT_BOOTSTRAP_CONNECTED, 23, 0, &[]);
            assert_eq!(status, STATUS_EVENT);
            assert_eq!(bootstrap.kind, OUTCOME_WRITE_FRAMES);
            assert_ne!(bootstrap.value0, 0, "bootstrap deadline token is explicit");
            let initial_tag = if initial_reason == 4 {
                EVENT_TIMER_FIRED
            } else {
                EVENT_ELIGIBILITY_LOST
            };
            let initial_token = if initial_reason == 4 {
                bootstrap.value0
            } else {
                0
            };
            let (status, terminal, _) = drive_ffi(handle, initial_tag, 0, initial_token, &[]);
            assert_eq!(status, STATUS_TERMINAL);
            assert_eq!(terminal.close_reason, initial_reason);

            let stale = handle;
            let barrier = Arc::new(Barrier::new(2));
            *DRIVE_AFTER_ARBITRATION_HOOK
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((handle, barrier.clone()));
            let worker = thread::spawn(move || {
                let late = EventV1 {
                    abi_version: ABI_VERSION,
                    struct_size: std::mem::size_of::<EventV1>() as u32,
                    tag: EVENT_BOOTSTRAP_CONNECTED,
                    flags: 0,
                    stream_id: 23,
                    write_token: 0,
                    bytes: std::ptr::null(),
                    bytes_len: 0,
                };
                let mut outcome = OutcomeV1::zeroed();
                let status = unsafe { apppilotkit_tp_v1_drive(stale, &late, &mut outcome) };
                (status, outcome)
            });
            barrier.wait();
            let internal = EventV1 {
                abi_version: ABI_VERSION,
                struct_size: std::mem::size_of::<EventV1>() as u32,
                tag: EVENT_INTERNAL_ERROR,
                flags: 0,
                stream_id: 0,
                write_token: 0,
                bytes: std::ptr::null(),
                bytes_len: 0,
            };
            let mut ignored = OutcomeV1::zeroed();
            assert_eq!(
                unsafe { apppilotkit_tp_v1_drive(handle, &internal, &mut ignored) },
                STATUS_BUSY
            );
            barrier.wait();
            let (status, outcome) = worker.join().expect("late drive");
            assert_eq!(status, STATUS_TERMINAL);
            assert_eq!(outcome.close_reason, initial_reason);
            assert_eq!(outcome.output, 0);
            *DRIVE_AFTER_ARBITRATION_HOOK
                .get()
                .unwrap()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

            let (_, late, _) = drive_ffi(handle, EVENT_BOOTSTRAP_CONNECTED, 29, 0, &[]);
            assert_eq!(late.close_reason, initial_reason);
            assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
        }
    }

    #[test]
    fn busy_latched_internal_error_survives_a_malformed_internal_error_owner() {
        let _serial = CONCURRENCY_HOOK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut handle, _) = create();
        let stale = handle;
        let barrier = Arc::new(Barrier::new(2));
        *DRIVE_AFTER_ARBITRATION_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((handle, barrier.clone()));
        let worker = thread::spawn(move || {
            let malformed = EventV1 {
                abi_version: ABI_VERSION,
                struct_size: std::mem::size_of::<EventV1>() as u32,
                tag: EVENT_INTERNAL_ERROR,
                flags: 0,
                stream_id: 1,
                write_token: 0,
                bytes: std::ptr::null(),
                bytes_len: 0,
            };
            let mut outcome = OutcomeV1::zeroed();
            let status = unsafe { apppilotkit_tp_v1_drive(stale, &malformed, &mut outcome) };
            (status, outcome)
        });
        barrier.wait();
        let internal = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_INTERNAL_ERROR,
            flags: 0,
            stream_id: 0,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let mut ignored = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &internal, &mut ignored) },
            STATUS_BUSY
        );
        barrier.wait();
        let (status, outcome) = worker.join().expect("malformed drive");
        assert_eq!(status, STATUS_TERMINAL);
        assert_eq!(outcome.kind, OUTCOME_LEASE_TERMINAL);
        assert_eq!(outcome.close_reason, 13);
        assert_eq!(outcome.output, 0);
        *DRIVE_AFTER_ARBITRATION_HOOK
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        let (_, late, _) = drive_ffi(handle, EVENT_BOOTSTRAP_CONNECTED, 29, 0, &[]);
        assert_eq!(late.close_reason, 13);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn busy_invalid_terminal_request_does_not_latch() {
        let (mut handle, _) = create();
        let entry = registry_lock(supervisors())
            .get(handle)
            .cloned()
            .expect("entry");
        entry.busy.store(true, Ordering::Release);
        let invalid = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_ELIGIBILITY_LOST,
            flags: 0,
            stream_id: 42,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let mut outcome = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &invalid, &mut outcome) },
            STATUS_BUSY
        );
        assert_eq!(entry.pending_terminal.load(Ordering::Acquire), 0);
        entry.busy.store(false, Ordering::Release);
        let bootstrap = EventV1 {
            tag: EVENT_BOOTSTRAP_CONNECTED,
            stream_id: 9,
            ..invalid
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &bootstrap, &mut outcome) },
            STATUS_EVENT
        );
        assert_eq!(outcome.kind, OUTCOME_WRITE_FRAMES);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn busy_timer_is_latched_and_cannot_be_overtaken() {
        let (mut handle, _) = create();
        let (status, bootstrap, _) = drive_ffi(handle, EVENT_BOOTSTRAP_CONNECTED, 9, 0, &[]);
        assert_eq!(status, STATUS_EVENT);
        assert_eq!(bootstrap.kind, OUTCOME_WRITE_FRAMES);
        assert_ne!(bootstrap.value0, 0, "bootstrap deadline token is explicit");
        let entry = registry_lock(supervisors())
            .get(handle)
            .cloned()
            .expect("entry");
        entry.busy.store(true, Ordering::Release);
        let timer = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_TIMER_FIRED,
            flags: 0,
            stream_id: 0,
            write_token: bootstrap.value0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let mut outcome = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &timer, &mut outcome) },
            STATUS_BUSY
        );
        assert_eq!(
            entry.pending_timer.load(Ordering::Acquire),
            bootstrap.value0
        );
        entry.busy.store(false, Ordering::Release);
        let bootstrap = EventV1 {
            tag: EVENT_BOOTSTRAP_CONNECTED,
            stream_id: 9,
            write_token: 0,
            ..timer
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &bootstrap, &mut outcome) },
            STATUS_TERMINAL
        );
        assert_eq!(outcome.kind, OUTCOME_LEASE_TERMINAL);
        assert_eq!(outcome.close_reason, 4);
        assert_eq!(outcome.output, 0);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn busy_unknown_timer_cannot_swallow_a_write_outcome() {
        let (mut handle, _) = create();
        let entry = registry_lock(supervisors())
            .get(handle)
            .cloned()
            .expect("entry");
        entry.busy.store(true, Ordering::Release);
        let unknown = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_TIMER_FIRED,
            flags: 0,
            stream_id: 0,
            write_token: 999,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let mut outcome = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &unknown, &mut outcome) },
            STATUS_BUSY
        );
        assert_eq!(entry.pending_timer.load(Ordering::Acquire), 0);
        entry.busy.store(false, Ordering::Release);
        let bootstrap = EventV1 {
            tag: EVENT_BOOTSTRAP_CONNECTED,
            stream_id: 29,
            write_token: 0,
            ..unknown
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &bootstrap, &mut outcome) },
            STATUS_EVENT
        );
        assert_eq!(outcome.kind, OUTCOME_WRITE_FRAMES);
        assert_ne!(outcome.output, 0);
        let mut output = outcome.output;
        assert_eq!(
            unsafe { apppilotkit_tp_v1_output_drop(&mut output) },
            STATUS_OK
        );
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn busy_lease_stream_failure_is_latched_without_serializing_other_handles() {
        let (mut handle, _) = create();
        let (_, m1, _) = drive_ffi(handle, EVENT_BOOTSTRAP_CONNECTED, 7, 0, &[]);
        let entry = registry_lock(supervisors())
            .get(handle)
            .cloned()
            .expect("entry");
        assert_eq!(entry.lease_stream.load(Ordering::Acquire), 7);
        entry.busy.store(true, Ordering::Release);
        let failure = EventV1 {
            abi_version: ABI_VERSION,
            struct_size: std::mem::size_of::<EventV1>() as u32,
            tag: EVENT_STREAM_IO_FAILED,
            flags: 0,
            stream_id: 7,
            write_token: 0,
            bytes: std::ptr::null(),
            bytes_len: 0,
        };
        let mut outcome = OutcomeV1::zeroed();
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &failure, &mut outcome) },
            STATUS_BUSY
        );
        entry.busy.store(false, Ordering::Release);
        let commit = EventV1 {
            tag: EVENT_FULL_WRITE_COMMITTED,
            write_token: m1.write_token,
            ..failure
        };
        assert_eq!(
            unsafe { apppilotkit_tp_v1_drive(handle, &commit, &mut outcome) },
            STATUS_TERMINAL
        );
        assert_eq!(outcome.kind, OUTCOME_LEASE_TERMINAL);
        assert_eq!(outcome.close_reason, 9);
        assert_eq!(unsafe { apppilotkit_tp_v1_drop(&mut handle) }, STATUS_OK);
    }

    #[test]
    fn canonical_header_and_export_controls_have_frozen_fingerprints() {
        let header = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/include/apppilotkit_transport.h"
        ));
        let apple = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/apppilotkit_transport.exports"
        ));
        let android = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/apppilotkit_transport.map"
        ));
        assert_eq!(
            format!("{:x}", Sha256::digest(header)),
            "b0bffdbfc2b5179de3f59f83e6abc2a59ed8d7d24f049caf806b176a53bc68f4"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(apple)),
            "db388adf31a93d6f125ef312cb186373890b4e2986df84ad47a167380ba068e5"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(android)),
            "bfd5c947a1763f998e2ed95a011c8c65e4a8ff673976eb1d620096de67ee8e7f"
        );
        let header = std::str::from_utf8(header).expect("ASCII C header");
        for symbol in [
            "apppilotkit_tp_v1_abi_version",
            "apppilotkit_tp_v1_create",
            "apppilotkit_tp_v1_drive",
            "apppilotkit_tp_v1_close",
            "apppilotkit_tp_v1_drop",
            "apppilotkit_tp_v1_output_count",
            "apppilotkit_tp_v1_output_len",
            "apppilotkit_tp_v1_output_copy",
            "apppilotkit_tp_v1_output_drop",
        ] {
            assert!(header.contains(symbol), "header missing {symbol}");
        }
    }
}
