use apppilotkit_rust_foundation_spike::{
    CommandOutput, CommandRequest, CommandRunner, PlatformTools, TokioCommandRunner,
};
use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;

#[derive(Default)]
struct RecordingRunner {
    requests: Mutex<Vec<CommandRequest>>,
}

#[tokio::test]
#[ignore = "host verification requires Xcode and Android platform-tools"]
async fn installed_platform_tools_run_without_a_connected_device() {
    let runner = TokioCommandRunner::default();
    let xcrun = std::env::var_os("APPPILOTKIT_XCRUN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/xcrun"));
    let adb = std::env::var_os("APPPILOTKIT_ADB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/homebrew/bin/adb"));
    let results = PlatformTools::new(&runner, xcrun, adb).probe().await;

    assert_eq!(results.len(), 4);
    assert!(
        results.iter().all(|result| result.output.success),
        "host platform probe failures: {results:#?}"
    );
}

impl CommandRunner for RecordingRunner {
    fn run<'a>(
        &'a self,
        request: CommandRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, String>> + Send + 'a>> {
        self.requests.lock().expect("request lock").push(request);
        Box::pin(async {
            Ok(CommandOutput {
                success: true,
                stdout: b"fixture version\n".to_vec(),
                stderr: Vec::new(),
            })
        })
    }
}

#[tokio::test]
async fn platform_probes_use_injected_paths_and_exact_device_free_arguments() {
    let runner = RecordingRunner::default();
    let tools = PlatformTools::new(
        &runner,
        PathBuf::from("/injected/xcrun"),
        PathBuf::from("/injected/adb"),
    );
    let results = tools.probe().await;

    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|result| result.output.success));
    let requests = runner.requests.into_inner().expect("request lock");
    assert_eq!(
        requests,
        vec![
            CommandRequest::new("/injected/xcrun", ["devicectl", "--version"]),
            CommandRequest::new("/injected/xcrun", ["devicectl", "help"]),
            CommandRequest::new("/injected/xcrun", ["simctl", "help"]),
            CommandRequest::new("/injected/adb", ["version"]),
        ]
    );
    assert_eq!(
        requests[0].args,
        vec![OsString::from("devicectl"), OsString::from("--version")]
    );
}
