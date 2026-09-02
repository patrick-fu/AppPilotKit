//! Publish-disabled raw platform launch seam.
//!
//! It moves selected-Target facts, public launch bytes, bounded deadlines,
//! cancellation, raw I/O, and platform failures only.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::Platform;

/// Secret-free closed platform failure classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformFailureKind {
    TimedOut,
    Cancelled,
    Eof,
    Unavailable,
    Rejected,
    CleanupFailed,
    Internal,
}

/// Opaque, secret-free platform-side failure.
///
/// This carrier intentionally implements neither `Debug` nor `Display`: callers
/// can branch only on its closed, non-sensitive classification.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct PlatformFailure {
    kind: PlatformFailureKind,
    primary_kind: PlatformFailureKind,
}

impl PlatformFailure {
    pub const fn new(kind: PlatformFailureKind) -> Self {
        Self {
            kind,
            primary_kind: kind,
        }
    }

    /// Records a cleanup failure without discarding the failure that required
    /// cleanup. This remains a closed, secret-free host-side classification.
    pub const fn cleanup_failed_after(primary_kind: PlatformFailureKind) -> Self {
        Self {
            kind: PlatformFailureKind::CleanupFailed,
            primary_kind,
        }
    }

    pub const fn kind(self) -> PlatformFailureKind {
        self.kind
    }

    pub const fn primary_kind(self) -> PlatformFailureKind {
        self.primary_kind
    }

    pub const fn cleanup_failed(self) -> bool {
        matches!(self.kind, PlatformFailureKind::CleanupFailed)
    }
}

/// Bounded absolute deadline supplied by the Host.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct AbsoluteDeadline {
    value: u64,
}

impl AbsoluteDeadline {
    pub fn new(value: u64) -> Result<Self, PlatformFailure> {
        if value == 0 {
            return Err(PlatformFailure::new(PlatformFailureKind::Rejected));
        }
        Ok(Self { value })
    }

    pub const fn value(self) -> u64 {
        self.value
    }
}

/// Idempotent cancellation signal shared with a platform operation.
#[derive(Clone)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
}

impl Cancellation {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

/// Exact platform selection for one launch attempt.
pub struct TargetSelection {
    platform: Platform,
    device_selector: String,
    app_id: String,
    artifact_path: String,
    artifact_digest: [u8; 32],
}

impl TargetSelection {
    pub fn new(
        platform: Platform,
        device_selector: String,
        app_id: String,
        artifact_path: String,
        artifact_digest: [u8; 32],
    ) -> Result<Self, PlatformFailure> {
        if device_selector.is_empty() || app_id.is_empty() || artifact_path.is_empty() {
            return Err(PlatformFailure::new(PlatformFailureKind::Rejected));
        }
        Ok(Self {
            platform,
            device_selector,
            app_id,
            artifact_path,
            artifact_digest,
        })
    }

    pub const fn platform(&self) -> Platform {
        self.platform
    }
    pub fn device_selector(&self) -> &str {
        &self.device_selector
    }
    pub fn app_id(&self) -> &str {
        &self.app_id
    }
    pub fn artifact_path(&self) -> &str {
        &self.artifact_path
    }
    pub const fn artifact_digest(&self) -> [u8; 32] {
        self.artifact_digest
    }
}

enum LaunchEndpointKind {
    IosLoopback { port: u16 },
    AndroidLocalAbstract { name: String },
}

/// Typed platform endpoint validated before it crosses the raw seam.
pub struct LaunchEndpoint {
    kind: LaunchEndpointKind,
}

impl LaunchEndpoint {
    pub fn ios_loopback(port: u16) -> Result<Self, PlatformFailure> {
        if !(49_152..=65_535).contains(&port) {
            return Err(PlatformFailure::new(PlatformFailureKind::Rejected));
        }
        Ok(Self {
            kind: LaunchEndpointKind::IosLoopback { port },
        })
    }

    pub fn android_local_abstract(name: String) -> Result<Self, PlatformFailure> {
        if !(32..=96).contains(&name.len()) {
            return Err(PlatformFailure::new(PlatformFailureKind::Rejected));
        }
        Ok(Self {
            kind: LaunchEndpointKind::AndroidLocalAbstract { name },
        })
    }

    pub fn ios_port(&self) -> Option<u16> {
        match &self.kind {
            LaunchEndpointKind::IosLoopback { port } => Some(*port),
            LaunchEndpointKind::AndroidLocalAbstract { .. } => None,
        }
    }

    pub fn android_name(&self) -> Option<&str> {
        match &self.kind {
            LaunchEndpointKind::IosLoopback { .. } => None,
            LaunchEndpointKind::AndroidLocalAbstract { name } => Some(name),
        }
    }
}

/// Already-encoded D2 canonical public launch material.
pub struct PublicLaunchDescriptor {
    canonical_bytes: Vec<u8>,
}

impl PublicLaunchDescriptor {
    pub fn from_d2_canonical_bytes(canonical_bytes: Vec<u8>) -> Result<Self, PlatformFailure> {
        if canonical_bytes.is_empty() {
            return Err(PlatformFailure::new(PlatformFailureKind::Rejected));
        }
        Ok(Self { canonical_bytes })
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }
}

/// Starts platform launch and exposes its endpoint before public launch material.
pub trait PlatformTargetAdapter: Send + Sync {
    fn begin_launch(
        &self,
        selection: TargetSelection,
        absolute_deadline: AbsoluteDeadline,
    ) -> Box<dyn PendingLaunch>;
}

/// One platform launch that can be completed exactly once.
///
/// After `begin_launch` succeeds, a descriptor encoding failure must consume
/// [`Self::abort`]. `launch` returning `Err` must have already cleaned owned
/// resources or return [`PlatformFailureKind::CleanupFailed`]. `Drop` must not
/// perform I/O; callers must consume this owner through `launch` or `abort`.
pub trait PendingLaunch: Send {
    fn endpoint(&self) -> &LaunchEndpoint;

    fn launch(
        self: Box<Self>,
        descriptor: PublicLaunchDescriptor,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<LaunchedTargetIo, PlatformFailure>;

    /// Consumes the unique launch owner and tears down reserved resources.
    ///
    /// Implementations must either finish cleanup successfully or return
    /// [`PlatformFailureKind::CleanupFailed`].
    fn abort(
        self: Box<Self>,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure>;
}

/// Connector for a raw Target stream.
pub trait RawConnector: Send + Sync {
    fn connect(
        &self,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<Arc<dyn RawDuplex>, PlatformFailure>;
}

/// Raw byte stream. `cancel` must be prompt and idempotent.
pub trait RawDuplex: Send + Sync {
    fn read(
        &self,
        output: &mut [u8],
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<usize, PlatformFailure>;
    fn write(
        &self,
        input: &[u8],
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<usize, PlatformFailure>;
    fn cancel(&self);
}

/// Adapter-owned cleanup that can be invoked once after launch ownership transfers.
pub trait CleanupReceipt: Send {
    fn cleanup(
        self: Box<Self>,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure>;
}

/// Raw Target I/O and its matching cleanup receipt.
pub struct LaunchedTargetIo {
    bootstrap: Arc<dyn RawDuplex>,
    connector: Arc<dyn RawConnector>,
    cleanup: Box<dyn CleanupReceipt>,
}

impl LaunchedTargetIo {
    pub fn new(
        bootstrap: Arc<dyn RawDuplex>,
        connector: Arc<dyn RawConnector>,
        cleanup: Box<dyn CleanupReceipt>,
    ) -> Self {
        Self {
            bootstrap,
            connector,
            cleanup,
        }
    }

    pub fn into_parts(
        self,
    ) -> (
        Arc<dyn RawDuplex>,
        Arc<dyn RawConnector>,
        Box<dyn CleanupReceipt>,
    ) {
        (self.bootstrap, self.connector, self.cleanup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deadline() -> AbsoluteDeadline {
        AbsoluteDeadline::new(1).unwrap_or_else(|_| panic!("nonzero deadline"))
    }

    #[test]
    fn platform_failure_kinds_are_closed_and_readable() {
        for kind in [
            PlatformFailureKind::TimedOut,
            PlatformFailureKind::Cancelled,
            PlatformFailureKind::Eof,
            PlatformFailureKind::Unavailable,
            PlatformFailureKind::Rejected,
            PlatformFailureKind::CleanupFailed,
            PlatformFailureKind::Internal,
        ] {
            assert_eq!(PlatformFailure::new(kind).kind(), kind);
            assert_eq!(PlatformFailure::new(kind).primary_kind(), kind);
            assert_eq!(
                PlatformFailure::new(kind).cleanup_failed(),
                kind == PlatformFailureKind::CleanupFailed
            );
        }
    }

    #[test]
    fn cleanup_failure_keeps_the_secret_free_primary_kind() {
        let failure = PlatformFailure::cleanup_failed_after(PlatformFailureKind::TimedOut);
        assert_eq!(failure.kind(), PlatformFailureKind::CleanupFailed);
        assert_eq!(failure.primary_kind(), PlatformFailureKind::TimedOut);
        assert!(failure.cleanup_failed());
    }

    struct AbortProbe {
        endpoint: LaunchEndpoint,
    }

    impl PendingLaunch for AbortProbe {
        fn endpoint(&self) -> &LaunchEndpoint {
            &self.endpoint
        }

        fn launch(
            self: Box<Self>,
            _: PublicLaunchDescriptor,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<LaunchedTargetIo, PlatformFailure> {
            Err(PlatformFailure::new(PlatformFailureKind::Rejected))
        }

        fn abort(
            self: Box<Self>,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<(), PlatformFailure> {
            Ok(())
        }
    }

    #[test]
    fn pending_launch_abort_is_consuming_and_implementable() {
        let pending: Box<dyn PendingLaunch> = Box::new(AbortProbe {
            endpoint: LaunchEndpoint::ios_loopback(49_152)
                .unwrap_or_else(|_| panic!("dynamic port")),
        });
        pending
            .abort(Cancellation::new(), deadline())
            .unwrap_or_else(|_| panic!("abort consumes the unique owner"));
    }
}
