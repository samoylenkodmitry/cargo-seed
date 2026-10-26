//! `rustc-threads`: a `build.rustc-wrapper` that turns on rustc's parallel
//! front end on the stable compiler.
//!
//! It adds `-Zthreads` only where it pays and changes nothing the user ships:
//! workspace crates (`CARGO_PRIMARY_PACKAGE`) in unoptimized builds. Release
//! builds and dependencies run as given. `RUSTC_BOOTSTRAP` names the one crate
//! being compiled, so no other crate, and no build-script probe, sees nightly.

use std::ffi::OsString;
use std::process::{Command, ExitCode};

const MAX_THREADS: usize = 8;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(program) = args.next() else {
        eprintln!("rustc-threads: run as build.rustc-wrapper");
        return ExitCode::FAILURE;
    };
    let args: Vec<OsString> = args.collect();
    let mut command = Command::new(program);
    command.args(&args);
    if let Some(crate_name) = parallel_crate(&args) {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(MAX_THREADS));
        command
            .env("RUSTC_BOOTSTRAP", crate_name)
            .arg(format!("-Zthreads={threads}"));
    }
    run(command)
}

/// The crate to compile with the parallel front end, if this call is one.
fn parallel_crate(args: &[OsString]) -> Option<String> {
    std::env::var_os("CARGO_PRIMARY_PACKAGE")?;
    if std::env::var_os("RUSTC_BOOTSTRAP").is_some() {
        return None;
    }
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let optimized = args.iter().enumerate().any(|(at, arg)| {
        let setting = match arg.as_str() {
            "-C" => args.get(at + 1).map_or("", String::as_str),
            _ => arg.strip_prefix("-C").unwrap_or(""),
        };
        setting
            .strip_prefix("opt-level=")
            .is_some_and(|level| level != "0")
    });
    if optimized || args.iter().any(|arg| arg.starts_with("-Zthreads")) {
        return None;
    }
    let crate_name = args
        .iter()
        .zip(args.iter().skip(1))
        .find(|(flag, _)| *flag == "--crate-name")?
        .1;
    // Cargo names the crate it compiles; a build script's probe has no such variable.
    (std::env::var("CARGO_CRATE_NAME").ok()? == *crate_name).then(|| crate_name.clone())
}

#[cfg(unix)]
fn run(mut command: Command) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let error = command.exec();
    eprintln!("rustc-threads: {error}");
    ExitCode::FAILURE
}

#[cfg(not(unix))]
fn run(mut command: Command) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("rustc-threads: {error}");
            ExitCode::FAILURE
        }
    }
}
