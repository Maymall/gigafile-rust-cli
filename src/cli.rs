// SPDX-License-Identifier: GPL-3.0-only

use std::{env, path::PathBuf, time::Duration};

use clap::{ArgAction, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{download, error::GfileError, info, jsonout, upload};

#[derive(Debug, Parser)]
#[command(
    name = "rgfile",
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
    /// Show metadata for a public GigaFile page without downloading files.
    Info {
        /// Download page URL.
        url: String,

        /// Per-request timeout in seconds.
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

        /// Print one JSON object.
        #[arg(long = "json")]
        json: bool,

        /// Disable non-error status output.
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,
    },
    /// Upload a local file.
    Upload {
        /// File to upload.
        file: PathBuf,

        /// File lifetime in days.
        #[arg(long = "lifetime", default_value_t = 100)]
        lifetime: u16,

        /// Upload chunk size, for example 50M or 1G.
        #[arg(long = "chunk-size", default_value = "100MiB")]
        chunk_size: String,

        /// Skip post-upload size verification.
        #[arg(long = "no-verify")]
        no_verify: bool,

        /// Idle timeout in seconds while uploading a chunk.
        #[arg(long = "timeout", default_value_t = 60)]
        timeout: u64,

        /// Retry count for retryable network/server failures.
        #[arg(long = "retries", default_value_t = 3)]
        retries: u32,

        /// Override the default User-Agent.
        #[arg(long = "user-agent")]
        user_agent: Option<String>,

        /// Save the fetched upload landing page HTML for diagnostics.
        #[arg(long = "dump-page")]
        dump_page: Option<PathBuf>,

        /// Print one JSON object and disable progress output.
        #[arg(long = "json")]
        json: bool,

        /// Disable progress and non-error status output.
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,
    },
}

pub fn init_tracing(verbosity: u8) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbosity {
            0 => "warn",
            1 => "info",
            _ => "debug",
        };
        EnvFilter::new(format!("rgfile={level}"))
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
        Commands::Info {
            url,
            timeout,
            retries,
            user_agent,
            dump_page,
            json,
            quiet,
        } => {
            let result = info::info(info::InfoOptions {
                url,
                timeout: Duration::from_secs(timeout),
                retries,
                user_agent,
                dump_page,
                allow_any_host: test_allow_any_host(),
            })
            .await;

            match result {
                Ok(report) => {
                    if json {
                        jsonout::print_info_report(&report)?;
                    } else if !quiet {
                        print_human_info_report(&report);
                    }
                    Ok(RunOutcome::Success)
                }
                Err(error) if json => {
                    let code = error.exit_code();
                    jsonout::print_error(&error)?;
                    Ok(RunOutcome::Failure(code))
                }
                Err(error) => Err(error),
            }
        }
        Commands::Upload {
            file,
            lifetime,
            chunk_size,
            no_verify,
            timeout,
            retries,
            user_agent,
            dump_page,
            json,
            quiet,
        } => {
            let result = upload::upload(upload::UploadOptions {
                file,
                lifetime,
                chunk_size: upload::parse_chunk_size(&chunk_size)?,
                verify: !no_verify,
                timeout: Duration::from_secs(timeout),
                retries,
                user_agent,
                dump_page,
                quiet: quiet || json,
                allow_any_host: test_allow_any_host(),
                entry_url: upload_entry_url(),
            })
            .await;

            match result {
                Ok(report) => {
                    if json {
                        jsonout::print_upload_report(&report)?;
                    } else if !quiet {
                        print_human_upload_report(&report);
                        if report.verified == Some(true) {
                            eprintln!("Verified upload size: {} bytes", report.bytes);
                        } else if report.verified.is_none() && !no_verify {
                            eprintln!("Warning: upload size verification was unavailable.");
                        }
                    }
                    Ok(RunOutcome::Success)
                }
                Err(error) if json => {
                    let code = error.exit_code();
                    jsonout::print_error(&error)?;
                    Ok(RunOutcome::Failure(code))
                }
                Err(error) => Err(error),
            }
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

fn print_human_info_report(report: &info::InfoReport) {
    println!("kind\t{}", page_kind_name(report.kind));
    println!("key_required\t{}", report.key_required);
    for file in &report.files {
        println!("display_name (may be masked)\t{}", file.display_name);
        if let Some(size) = &file.display_size {
            println!("display_size\t{size}");
        }
        if let Some(bytes) = file.approx_bytes {
            println!("approx_bytes\t{bytes}");
        }
    }
}

fn print_human_upload_report(report: &upload::UploadReport) {
    println!("{}", report.url);
    if let Some(delkey) = &report.delkey {
        println!("delete_key={delkey}");
        eprintln!("Warning: save this delete key; it is required to delete the uploaded file.");
    }
    if let Some(filename) = &report.remote_filename {
        println!("remote_filename={filename}");
    }
    if let Some(expires_at) = &report.expires_at_estimate {
        println!("expires_at_estimate={expires_at}");
    }
}

fn page_kind_name(kind: crate::parser::download::PageKind) -> &'static str {
    match kind {
        crate::parser::download::PageKind::Single => "single",
        crate::parser::download::PageKind::Matomete => "matomete",
    }
}

fn test_allow_any_host() -> bool {
    env::var("GFILE_TEST_ALLOW_ANY_HOST").as_deref() == Ok("1")
}

fn upload_entry_url() -> String {
    env::var("GFILE_TEST_ENTRY_URL").unwrap_or_else(|_| upload::default_entry_url().to_owned())
}
