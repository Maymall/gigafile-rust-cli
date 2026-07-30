// SPDX-License-Identifier: MIT

use std::process::ExitCode;

use clap::{Parser, error::ErrorKind};
use rgfile::cli::{self, Cli, RunOutcome};

fn main() -> ExitCode {
    restore_sigpipe_default();

    let json_requested = std::env::args_os().any(|arg| arg == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let code = err.exit_code();
            let is_display = matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            if json_requested && !is_display {
                let _ = rgfile::jsonout::print_usage_error(err.to_string());
            } else {
                let _ = err.print();
            }
            return exit_code(code);
        }
    };
    let json_output = cli.wants_json();

    cli::init_tracing(cli.verbose);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("failed to start async runtime: {err}");
            return exit_code(1);
        }
    };

    match runtime.block_on(cli::run(cli)) {
        Ok(RunOutcome::Success) => ExitCode::SUCCESS,
        Ok(RunOutcome::Failure(code)) => exit_code(i32::from(code)),
        Err(err) => {
            if json_output {
                let _ = rgfile::jsonout::print_error(&err);
            } else {
                eprintln!("{}", err.user_message());
            }
            exit_code(i32::from(err.exit_code()))
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(unix)]
fn restore_sigpipe_default() {
    // SAFETY: This only restores the process SIGPIPE disposition to the OS
    // default before worker threads are started; no Rust references are shared.
    unsafe {
        let _ = libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe_default() {}
