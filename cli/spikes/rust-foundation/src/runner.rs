use crate::{ProcessSpec, Termination, run_process};
use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl CommandRequest {
    #[must_use]
    pub fn new<P, I, A>(program: P, args: I) -> Self
    where
        P: Into<PathBuf>,
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait CommandRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        request: CommandRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, String>> + Send + 'a>>;
}

#[derive(Debug, Clone)]
pub struct TokioCommandRunner {
    timeout: Duration,
    capture_limit: usize,
}

impl TokioCommandRunner {
    #[must_use]
    pub fn new(timeout: Duration, capture_limit: usize) -> Self {
        Self {
            timeout,
            capture_limit,
        }
    }
}

impl Default for TokioCommandRunner {
    fn default() -> Self {
        Self::new(Duration::from_secs(10), 1024 * 1024)
    }
}

impl CommandRunner for TokioCommandRunner {
    fn run<'a>(
        &'a self,
        request: CommandRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, String>> + Send + 'a>> {
        Box::pin(async move {
            let outcome = run_process(
                ProcessSpec {
                    program: request.program,
                    args: request.args,
                    timeout: self.timeout,
                    term_grace: Duration::from_millis(250),
                    capture_limit: self.capture_limit,
                },
                CancellationToken::new(),
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok(CommandOutput {
                success: outcome.termination == Termination::ExitCode(0),
                stdout: outcome.stdout.bytes,
                stderr: outcome.stderr.bytes,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformProbe {
    pub name: &'static str,
    pub output: CommandOutput,
}

pub struct PlatformTools<'a, R> {
    runner: &'a R,
    xcrun: PathBuf,
    adb: PathBuf,
}

impl<'a, R> PlatformTools<'a, R>
where
    R: CommandRunner,
{
    #[must_use]
    pub fn new(runner: &'a R, xcrun: PathBuf, adb: PathBuf) -> Self {
        Self { runner, xcrun, adb }
    }

    pub async fn probe(&self) -> Vec<PlatformProbe> {
        let requests = [
            (
                "devicectl-version",
                CommandRequest::new(&self.xcrun, ["devicectl", "--version"]),
            ),
            (
                "devicectl-help",
                CommandRequest::new(&self.xcrun, ["devicectl", "help"]),
            ),
            (
                "simctl-help",
                CommandRequest::new(&self.xcrun, ["simctl", "help"]),
            ),
            ("adb-version", CommandRequest::new(&self.adb, ["version"])),
        ];
        let mut probes = Vec::with_capacity(requests.len());
        for (name, request) in requests {
            let output = self
                .runner
                .run(request)
                .await
                .unwrap_or_else(|error| CommandOutput {
                    success: false,
                    stdout: Vec::new(),
                    stderr: error.into_bytes(),
                });
            probes.push(PlatformProbe { name, output });
        }
        probes
    }
}
