use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub term_grace: Duration,
    pub capture_limit: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CompletionReason {
    Completed,
    Cancelled { forced: bool },
    TimedOut { forced: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Termination {
    ExitCode(i32),
    Signal(i32),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Capture {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProcessOutcome {
    pub reason: CompletionReason,
    pub termination: Termination,
    pub stdout: Capture,
    pub stderr: Capture,
}

#[derive(Debug)]
pub enum ProcessError {
    Spawn(std::io::Error),
    Wait(std::io::Error),
    OutputTask(tokio::task::JoinError),
    MissingPipe(&'static str),
    MissingPid,
    ReapTimedOut,
    ProcessGroupRemained,
    OutputDrainTimedOut(&'static str),
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "failed to spawn child: {error}"),
            Self::Wait(error) => write!(formatter, "failed to wait for child: {error}"),
            Self::OutputTask(error) => write!(formatter, "output reader failed: {error}"),
            Self::MissingPipe(stream) => write!(formatter, "child {stream} pipe was not captured"),
            Self::MissingPid => formatter.write_str("spawned child had no process id"),
            Self::ReapTimedOut => formatter.write_str("timed out reaping the direct child"),
            Self::ProcessGroupRemained => {
                formatter.write_str("managed process group still had members after forced cleanup")
            }
            Self::OutputDrainTimedOut(stream) => {
                write!(formatter, "timed out draining child {stream}")
            }
        }
    }
}

impl std::error::Error for ProcessError {}

pub async fn run_process(
    spec: ProcessSpec,
    cancellation: CancellationToken,
) -> Result<ProcessOutcome, ProcessError> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().map_err(ProcessError::Spawn)?;
    let group = child
        .id()
        .and_then(|pid| Pid::from_raw(pid as i32))
        .ok_or(ProcessError::MissingPid)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(ProcessError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(ProcessError::MissingPipe("stderr"))?;
    let stdout_task = tokio::spawn(capture_stream(stdout, spec.capture_limit));
    let stderr_task = tokio::spawn(capture_stream(stderr, spec.capture_limit));

    let event = {
        let wait = child.wait();
        tokio::pin!(wait);
        tokio::select! {
            status = &mut wait => ProcessEvent::Exited(status.map_err(ProcessError::Wait)?),
            () = cancellation.cancelled() => ProcessEvent::Cancelled,
            () = tokio::time::sleep(spec.timeout) => ProcessEvent::TimedOut,
        }
    };

    let (status, reason) = match event {
        ProcessEvent::Exited(status) => {
            if test_kill_process_group(group).is_ok() {
                let _ = kill_process_group(group, Signal::KILL);
                require_group_exit(group).await?;
            }
            (status, CompletionReason::Completed)
        }
        ProcessEvent::Cancelled => {
            let (status, forced) =
                terminate_process_group(&mut child, group, spec.term_grace).await?;
            (status, CompletionReason::Cancelled { forced })
        }
        ProcessEvent::TimedOut => {
            let (status, forced) =
                terminate_process_group(&mut child, group, spec.term_grace).await?;
            (status, CompletionReason::TimedOut { forced })
        }
    };

    let stdout = finish_capture(stdout_task, "stdout").await?;
    let stderr = finish_capture(stderr_task, "stderr").await?;
    Ok(ProcessOutcome {
        reason,
        termination: termination(status),
        stdout,
        stderr,
    })
}

enum ProcessEvent {
    Exited(ExitStatus),
    Cancelled,
    TimedOut,
}

async fn terminate_process_group(
    child: &mut Child,
    group: Pid,
    grace: Duration,
) -> Result<(ExitStatus, bool), ProcessError> {
    let _ = kill_process_group(group, Signal::TERM);
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(status) => {
            let status = status.map_err(ProcessError::Wait)?;
            let forced = if test_kill_process_group(group).is_ok() {
                let _ = kill_process_group(group, Signal::KILL);
                true
            } else {
                false
            };
            require_group_exit(group).await?;
            Ok((status, forced))
        }
        Err(_) => {
            let _ = kill_process_group(group, Signal::KILL);
            let _ = child.start_kill();
            let status = tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .map_err(|_| ProcessError::ReapTimedOut)?
                .map_err(ProcessError::Wait)?;
            require_group_exit(group).await?;
            Ok((status, true))
        }
    }
}

async fn require_group_exit(group: Pid) -> Result<(), ProcessError> {
    for _ in 0..100 {
        if test_kill_process_group(group).is_err() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(ProcessError::ProcessGroupRemained)
}

async fn capture_stream(mut stream: impl AsyncRead + Unpin, limit: usize) -> Capture {
    let mut capture = Capture {
        bytes: Vec::with_capacity(limit.min(8192)),
        truncated: false,
    };
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match stream.read(&mut buffer).await {
            Ok(read) => read,
            Err(_) => {
                capture.truncated = true;
                break;
            }
        };
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(capture.bytes.len());
        let retained = read.min(remaining);
        capture.bytes.extend_from_slice(&buffer[..retained]);
        capture.truncated |= retained < read;
    }
    capture
}

async fn finish_capture(
    task: JoinHandle<Capture>,
    stream: &'static str,
) -> Result<Capture, ProcessError> {
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .map_err(|_| ProcessError::OutputDrainTimedOut(stream))?
        .map_err(ProcessError::OutputTask)
}

#[cfg(unix)]
fn termination(status: ExitStatus) -> Termination {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        Termination::Signal(signal)
    } else {
        Termination::ExitCode(status.code().unwrap_or(1))
    }
}
