use std::{
    ffi::OsString,
    io::{self, Read},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use apppilotkit_host_runtime::adapter::{
    AbsoluteDeadline, Cancellation, PlatformFailure, PlatformFailureKind,
};

const OUTPUT_LIMIT: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(2);

pub(crate) struct ProcessOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) success: bool,
}

pub(crate) trait CommandRunner: Send + Sync {
    fn run(
        &self,
        executable: &Path,
        serial: &str,
        arguments: &[OsString],
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ProcessOutput, PlatformFailure>;
}

pub(crate) struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(
        &self,
        executable: &Path,
        serial: &str,
        arguments: &[OsString],
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ProcessOutput, PlatformFailure> {
        ensure_active(cancellation, deadline)?;
        let mut child = Command::new(executable)
            .arg("-s")
            .arg(serial)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| failure(PlatformFailureKind::Internal))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| failure(PlatformFailureKind::Internal))?;
        let oversized = Arc::new(AtomicBool::new(false));
        let stdout_reader = spawn_reader(stdout, Arc::clone(&oversized));
        let stderr_reader = spawn_reader(stderr, Arc::clone(&oversized));

        let status = loop {
            if cancellation.is_cancelled() {
                terminate(&mut child);
                join_reader(stdout_reader)?;
                join_reader(stderr_reader)?;
                return Err(failure(PlatformFailureKind::Cancelled));
            }
            if expired(deadline) {
                terminate(&mut child);
                join_reader(stdout_reader)?;
                join_reader(stderr_reader)?;
                return Err(failure(PlatformFailureKind::TimedOut));
            }
            if oversized.load(Ordering::Acquire) {
                terminate(&mut child);
                join_reader(stdout_reader)?;
                join_reader(stderr_reader)?;
                return Err(failure(PlatformFailureKind::Rejected));
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(_) => {
                    terminate(&mut child);
                    join_reader(stdout_reader)?;
                    join_reader(stderr_reader)?;
                    return Err(failure(PlatformFailureKind::Internal));
                }
            }
        };

        let stdout = join_reader(stdout_reader)?;
        let stderr = join_reader(stderr_reader)?;
        if oversized.load(Ordering::Acquire) {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        Ok(ProcessOutput {
            stdout,
            stderr,
            success: status.success(),
        })
    }
}

fn spawn_reader<R>(
    mut reader: R,
    oversized: Arc<AtomicBool>,
) -> thread::JoinHandle<Result<Vec<u8>, io::Error>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let count = reader.read(&mut chunk)?;
            if count == 0 {
                return Ok(output);
            }
            if output.len().saturating_add(count) > OUTPUT_LIMIT {
                oversized.store(true, Ordering::Release);
            } else {
                output.extend_from_slice(&chunk[..count]);
            }
        }
    })
}

fn join_reader(
    reader: thread::JoinHandle<Result<Vec<u8>, io::Error>>,
) -> Result<Vec<u8>, PlatformFailure> {
    reader
        .join()
        .map_err(|_| failure(PlatformFailureKind::Internal))?
        .map_err(|_| failure(PlatformFailureKind::Internal))
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn ensure_active(
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    if cancellation.is_cancelled() {
        return Err(failure(PlatformFailureKind::Cancelled));
    }
    if expired(deadline) {
        return Err(failure(PlatformFailureKind::TimedOut));
    }
    Ok(())
}

pub(crate) fn remaining(deadline: AbsoluteDeadline) -> Result<Duration, PlatformFailure> {
    let now = now_unix_ms()?;
    let millis = deadline.value().saturating_sub(now);
    if millis == 0 {
        return Err(failure(PlatformFailureKind::TimedOut));
    }
    Ok(Duration::from_millis(millis))
}

fn expired(deadline: AbsoluteDeadline) -> bool {
    match now_unix_ms() {
        Ok(now) => now >= deadline.value(),
        Err(_) => true,
    }
}

fn now_unix_ms() -> Result<u64, PlatformFailure> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| failure(PlatformFailureKind::Internal))?
            .as_millis(),
    )
    .map_err(|_| failure(PlatformFailureKind::Internal))
}

pub(crate) const fn failure(kind: PlatformFailureKind) -> PlatformFailure {
    PlatformFailure::new(kind)
}
