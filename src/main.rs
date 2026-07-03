// SPDX-License-Identifier: GPL-3.0-only

use std::process::ExitCode;

use clap::Parser;
use rgfile::cli::{self, Cli, RunOutcome};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let code = err.exit_code();
            let _ = err.print();
            return exit_code(code);
        }
    };

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
            eprintln!("{}", err.user_message());
            exit_code(i32::from(err.exit_code()))
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
