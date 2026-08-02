use apppilotkit_rust_foundation_spike::{CompletionReason, ProcessSpec, Termination, run_process};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn fixture(mode: &str) -> ProcessSpec {
    ProcessSpec {
        program: PathBuf::from(env!("CARGO_BIN_EXE_fixture-child")),
        args: vec![OsString::from(mode)],
        timeout: Duration::from_secs(3),
        term_grace: Duration::from_millis(100),
        capture_limit: 512 * 1024,
    }
}

#[tokio::test]
async fn captures_large_stdout_and_stderr_without_mixing_or_deadlock() {
    let outcome = run_process(fixture("streams"), CancellationToken::new())
        .await
        .expect("fixture should run");

    assert_eq!(outcome.reason, CompletionReason::Completed);
    assert_eq!(outcome.termination, Termination::ExitCode(0));
    assert!(!outcome.stdout.truncated);
    assert!(!outcome.stderr.truncated);
    let stdout = String::from_utf8(outcome.stdout.bytes).expect("stdout UTF-8");
    let stderr = String::from_utf8(outcome.stderr.bytes).expect("stderr UTF-8");
    assert!(stdout.starts_with("stdout:0000\n"));
    assert!(stdout.ends_with("stdout:9999\n"));
    assert!(stderr.starts_with("stderr:0000\n"));
    assert!(stderr.ends_with("stderr:9999\n"));
    assert!(!stdout.contains("stderr:"));
    assert!(!stderr.contains("stdout:"));
}

#[tokio::test]
async fn cancellation_forces_the_owned_process_group_and_reaps_the_child() {
    let ready_directory = tempfile::tempdir().expect("ready directory should be created");
    let ready = ready_directory.path().join("tree.ready");
    let mut spec = fixture("tree-root");
    spec.args.push(ready.as_os_str().to_owned());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let observed_ready = ready.clone();
    tokio::spawn(async move {
        for _ in 0..500 {
            if observed_ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        trigger.cancel();
    });
    let started = Instant::now();
    let outcome = run_process(spec, cancel)
        .await
        .expect("cancelled fixture should produce an outcome");

    assert!(ready.exists(), "fixture must be ready before cancellation");
    assert_eq!(
        outcome.reason,
        CompletionReason::Cancelled { forced: true },
        "unexpected cancellation outcome: {outcome:?}"
    );
    assert!(matches!(outcome.termination, Termination::Signal(9)));
    assert!(started.elapsed() < Duration::from_secs(2));
    let combined = [outcome.stdout.bytes, outcome.stderr.bytes].concat();
    let text = String::from_utf8(combined).expect("fixture PID output should be UTF-8");
    let pids = text
        .lines()
        .filter_map(|line| {
            line.split_once("pid=")
                .and_then(|(_, pid)| pid.parse::<i32>().ok())
        })
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 2, "expected root and descendant PIDs: {text}");
    for pid in pids {
        assert!(
            rustix::process::test_kill_process(
                rustix::process::Pid::from_raw(pid).expect("fixture PID must be positive"),
            )
            .is_err(),
            "fixture PID {pid} should not remain alive"
        );
    }
}

#[tokio::test]
async fn a_signal_exit_is_reported_as_a_signal_not_an_exit_code() {
    let outcome = run_process(fixture("signal-exit"), CancellationToken::new())
        .await
        .expect("signal fixture should run");

    assert_eq!(outcome.reason, CompletionReason::Completed);
    assert_eq!(outcome.termination, Termination::Signal(15));
}

#[tokio::test]
async fn timeout_uses_the_same_bounded_forced_cleanup_path() {
    let mut spec = fixture("tree-root");
    spec.timeout = Duration::from_secs(2);
    let started = Instant::now();
    let outcome = run_process(spec, CancellationToken::new())
        .await
        .expect("timed-out fixture should produce an outcome");

    assert_eq!(
        outcome.reason,
        CompletionReason::TimedOut { forced: true },
        "unexpected timeout outcome: {outcome:?}"
    );
    assert_eq!(outcome.termination, Termination::Signal(9));
    assert!(started.elapsed() < Duration::from_secs(3));
}
