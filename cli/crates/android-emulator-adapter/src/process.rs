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
        if !status.success() {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        Ok(ProcessOutput { stdout, stderr })
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_SCRIPT: AtomicU64 = AtomicU64::new(1);

    fn deadline_after(millis: u64) -> AbsoluteDeadline {
        let now = now_unix_ms().unwrap_or_else(|_| panic!("clock"));
        AbsoluteDeadline::new(now + millis).unwrap_or_else(|_| panic!("deadline"))
    }

    struct Script {
        directory: PathBuf,
        path: PathBuf,
    }

    impl Script {
        fn new(body: &str) -> Self {
            let unique = format!(
                "apppilotkit-android-process-{}-{}-{}",
                std::process::id(),
                now_unix_ms().unwrap_or(0),
                NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed)
            );
            let directory = std::env::temp_dir().join(unique);
            fs::create_dir(&directory).expect("script directory");
            let path = directory.join("fake-adb");
            fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("script");
            let mut permissions = fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&path, permissions).expect("permissions");
            Self { directory, path }
        }
    }

    fn child_pid(path: &std::path::Path) -> libc::pid_t {
        fs::read_to_string(path)
            .expect("child pid file")
            .trim()
            .parse()
            .expect("child pid")
    }

    fn assert_reaped(pid: libc::pid_t) {
        // SAFETY: signal 0 performs no mutation and only queries this exact PID.
        let result = unsafe { libc::kill(pid, 0) };
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    impl Drop for Script {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn process_deadline_terminates_the_exact_child() {
        let script = Script::new("exec sleep 5");
        let error = SystemCommandRunner
            .run(
                &script.path,
                "emulator-5554",
                &[OsString::from("get-state")],
                &Cancellation::new(),
                deadline_after(30),
            )
            .err()
            .expect("deadline");
        assert_eq!(error.kind(), PlatformFailureKind::TimedOut);
    }

    #[test]
    fn process_cancellation_is_prompt() {
        let pid_file = std::env::temp_dir().join(format!(
            "apppilotkit-cancel-pid-{}",
            NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed)
        ));
        let script = Script::new(&format!(
            "printf '%s' \"$$\" > '{}'; exec sleep 5",
            pid_file.display()
        ));
        let cancellation = Cancellation::new();
        let cancel_from_thread = cancellation.clone();
        let pid_from_thread = pid_file.clone();
        let cancel = thread::spawn(move || {
            while match fs::read_to_string(&pid_from_thread) {
                Ok(pid) => pid.trim().is_empty(),
                Err(_) => true,
            } {
                thread::yield_now();
            }
            cancel_from_thread.cancel();
        });
        let error = SystemCommandRunner
            .run(
                &script.path,
                "emulator-5554",
                &[OsString::from("get-state")],
                &cancellation,
                deadline_after(2_000),
            )
            .err()
            .expect("cancelled");
        cancel.join().expect("canceller");
        assert_eq!(error.kind(), PlatformFailureKind::Cancelled);
        assert_reaped(child_pid(&pid_file));
        fs::remove_file(pid_file).expect("remove pid file");
    }

    #[test]
    fn oversized_process_output_is_rejected_without_waiting_for_exit() {
        let pid_file = std::env::temp_dir().join(format!(
            "apppilotkit-oversize-pid-{}",
            NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed)
        ));
        let oversized = "x".repeat(OUTPUT_LIMIT + 1);
        let script = Script::new(&format!(
            "printf '%s' \"$$\" > '{}'; printf '%s' '{}'; exec sleep 5",
            pid_file.display(),
            oversized
        ));
        let error = SystemCommandRunner
            .run(
                &script.path,
                "emulator-5554",
                &[OsString::from("help")],
                &Cancellation::new(),
                deadline_after(2_000),
            )
            .err()
            .expect("oversized output");
        assert_eq!(error.kind(), PlatformFailureKind::Rejected);
        assert_reaped(child_pid(&pid_file));
        fs::remove_file(pid_file).expect("remove pid file");
    }
}
