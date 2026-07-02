// SPDX-License-Identifier: GPL-3.0-only

use std::{env, path::PathBuf, time::Duration};

use clap::{ArgAction, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{download, error::GfileError, jsonout};

#[derive(Debug, Parser)]
#[command(
    name = "gfile",
    version,
    about = "Upload and download GigaFile public web files",
    long_about = None
)]
pub struct Cli {
    #[arg(
        short = 'v',
        long = "verbose",
        action = ArgAction::Count,
        global = true,
        help = "Increase logging verbosity (-v for info, -vv for debug)"
    )]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Download a file from a public GigaFile page.
    Download {
        /// Download page URL.
        url: String,

        /// Output directory or explicit output file path.
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,

        /// Download key.
        #[arg(short = 'k', long = "key", visible_alias = "password")]
        key: Option<String>,

        /// Overwrite the final output file if it already exists.
        #[arg(long = "force")]
        force: bool,

        /// Ignore an existing .part file and start from zero.
        #[arg(long = "no-resume")]
        no_resume: bool,

        /// Per-read stall timeout in seconds.
        #[arg(long = "timeout", default_value_t = 60)]
        timeout: u64,

        /// Retry count for retryable network/server failures.
        #[arg(long = "retries", default_value_t = 3)]
        retries: u32,

        /// Override the default User-Agent.
        #[arg(long = "user-agent")]
        user_agent: Option<String>,

        /// Save the fetched download page HTML for diagnostics.
        #[arg(long = "dump-page")]
        dump_page: Option<PathBuf>,

        /// Print one JSON object and disable progress output.
        #[arg(long = "json")]
        json: bool,

        /// Disable progress and non-error status output.
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,
    },
    /// Upload a local file.
    Upload {
        /// File to upload.
        file: PathBuf,
    },
}

pub fn init_tracing(verbosity: u8) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbosity {
            0 => "warn",
            1 => "info",
            _ => "debug",
        };
        EnvFilter::new(format!("gfile_rust={level},gfile={level}"))
    });

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Success,
    Failure(u8),
}

pub async fn run(cli: Cli) -> Result<RunOutcome, GfileError> {
    match cli.command {
        Commands::Download {
            url,
            output,
            key,
            force,
            no_resume,
            timeout,
            retries,
            user_agent,
            dump_page,
            json,
            quiet,
        } => {
            let result = download::download(download::DownloadOptions {
                url,
                output,
                key,
                force,
                no_resume,
                timeout: Duration::from_secs(timeout),
                retries,
                user_agent,
                dump_page,
                quiet: quiet || json,
                allow_any_host: test_allow_any_host(),
            })
            .await;

            match result {
                Ok(report) => {
                    if json {
                        jsonout::print_download_report(&report)?;
                    } else if !quiet {
                        print_human_download_report(&report);
                    }
                    if let Some(code) = report.first_failure_exit_code() {
                        Ok(RunOutcome::Failure(code))
                    } else {
                        Ok(RunOutcome::Success)
                    }
                }
                Err(error) if json => {
                    let code = error.exit_code();
                    jsonout::print_error(&error)?;
                    Ok(RunOutcome::Failure(code))
                }
                Err(error) => Err(error),
            }
        }
        Commands::Upload { file: _ } => {
            eprintln!("upload is not implemented yet");
            Err(GfileError::UploadRejected {
                detail: "upload command is not implemented yet".to_owned(),
            })
        }
    }
}

fn print_human_download_report(report: &download::DownloadReport) {
    if report.kind == crate::parser::download::PageKind::Single {
        if let Some(path) = report.files.first().and_then(|file| file.path.as_ref()) {
            println!("{}", path.display());
        }
        return;
    }

    for file in &report.files {
        match (&file.path, &file.error) {
            (Some(path), None) => println!("ok\t{}\t{}", file.name, path.display()),
            (_, Some(error)) => println!("error\t{}\t{}", file.name, error.code),
            _ => println!("error\t{}\tunknown", file.name),
        }
    }
}

fn test_allow_any_host() -> bool {
    env::var("GFILE_TEST_ALLOW_ANY_HOST").as_deref() == Ok("1")
}
