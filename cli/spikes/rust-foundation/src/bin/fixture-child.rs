use rustix::process::{Signal, getpid, kill_process};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("streams") => emit_streams(),
        Some("tree-root") => tree_root().await,
        Some("tree-descendant") => tree_descendant().await,
        Some("signal-exit") => signal_exit(),
        _ => return ExitCode::from(2),
    }
    ExitCode::SUCCESS
}

fn emit_streams() {
    let mut stdout = BufWriter::new(std::io::stdout().lock());
    let mut stderr = BufWriter::new(std::io::stderr().lock());
    for index in 0..10_000 {
        writeln!(stdout, "stdout:{index:04}").expect("fixture stdout should be writable");
        writeln!(stderr, "stderr:{index:04}").expect("fixture stderr should be writable");
    }
    stdout.flush().expect("fixture stdout should flush");
    stderr.flush().expect("fixture stderr should flush");
}

async fn tree_root() {
    let terminate = term_listener();
    let ready = std::env::args().nth(2).map(PathBuf::from);
    let descendant_ready = ready.as_ref().map(|path| path.with_extension("descendant"));
    let mut descendant = tokio::process::Command::new(
        std::env::current_exe().expect("fixture executable should have a path"),
    )
    .arg("tree-descendant")
    .args(descendant_ready.iter())
    .spawn()
    .expect("fixture descendant should spawn");
    println!("root pid={}", std::process::id());
    std::io::stdout().flush().expect("root PID should flush");
    if let (Some(ready), Some(descendant_ready)) = (&ready, &descendant_ready) {
        while !descendant_ready.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        mark_ready(ready);
    }
    ignore_term(terminate).await;
    let _ = descendant.wait().await;
}

async fn tree_descendant() {
    let terminate = term_listener();
    let ready = std::env::args().nth(2).map(PathBuf::from);
    eprintln!("descendant pid={}", std::process::id());
    std::io::stderr()
        .flush()
        .expect("descendant PID should flush");
    if let Some(ready) = ready {
        mark_ready(&ready);
    }
    ignore_term(terminate).await;
}

fn mark_ready(path: &Path) {
    std::fs::write(path, b"ready\n").expect("fixture ready marker should be writable");
}

fn term_listener() -> tokio::signal::unix::Signal {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("fixture should register SIGTERM")
}

async fn ignore_term(mut terminate: tokio::signal::unix::Signal) {
    terminate.recv().await;
    std::future::pending::<()>().await;
}

fn signal_exit() {
    kill_process(getpid(), Signal::TERM).expect("fixture should signal itself");
}
