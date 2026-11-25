//! Standalone Rust child process used by supervisor unit tests.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().collect();
    let program = Path::new(&args[0]);
    if program.file_name().is_some_and(|p| p == "daemon") {
        std::thread::sleep(Duration::from_secs(60));
        return ExitCode::SUCCESS;
    }
    let scenario = fs::read_to_string(program.parent().unwrap().join("scenario")).unwrap();
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}.calls", args[0]))
            .unwrap(),
        "{}",
        args[1..].join(" ")
    )
    .unwrap();
    let action = args.get(1).map(String::as_str).unwrap_or("");
    let marker = format!("{}.checked", args[0]);
    let first = || {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&marker)
            .is_ok()
    };
    match scenario.as_str() {
        "once-not-ready" if action == "control" && first() => ExitCode::from(1),
        "fail-trigger" if action == "trigger" => ExitCode::from(9),
        "stall-settle" if action == "settle" => {
            std::thread::sleep(Duration::from_secs(60));
            ExitCode::SUCCESS
        }
        "stall-health" if !first() => {
            std::thread::sleep(Duration::from_secs(60));
            ExitCode::SUCCESS
        }
        "fail" => ExitCode::from(1),
        _ => ExitCode::SUCCESS,
    }
}
