// SPDX-License-Identifier: GPL-3.0-only

use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{ArgAction, Parser, Subcommand};
use serde::Serialize;
use tracing_subscriber::EnvFilter;

use crate::{
    config, download,
    error::{GfileError, IoOp},
    history::{self, HistoryOverride, HistoryRecord},
    info, jsonout, upload,
};

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
        help_heading = "Global Options",
        help = "Increase logging verbosity (-v for info, -vv for debug)"
    )]
    pub verbose: u8,

    #[arg(
        long = "config",
        value_name = "PATH",
        global = true,
        conflicts_with = "no_config",
        help_heading = "Global Options",
        help = "Load configuration from a specific TOML file"
    )]
    pub config: Option<PathBuf>,

    #[arg(
        long = "no-config",
        global = true,
        help_heading = "Global Options",
        help = "Do not load a configuration file"
    )]
    pub no_config: bool,

    #[arg(
        long = "history",
        global = true,
        conflicts_with = "no_history",
        help_heading = "Global Options",
        help = "Enable history for this run"
    )]
    pub history: bool,

    #[arg(
        long = "no-history",
        global = true,
        help_heading = "Global Options",
        help = "Disable history for this run"
    )]
    pub no_history: bool,

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

        /// Number of download connections for each file (1-16).
        #[arg(long = "threads")]
        threads: Option<u8>,

        /// Per-read stall timeout in seconds.
        #[arg(long = "timeout")]
        timeout: Option<u64>,

        /// Retry count for retryable network/server failures.
        #[arg(long = "retries")]
        retries: Option<u32>,

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
        #[arg(long = "timeout")]
        timeout: Option<u64>,

        /// Retry count for retryable network/server failures.
        #[arg(long = "retries")]
        retries: Option<u32>,

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
        #[arg(long = "lifetime")]
        lifetime: Option<u16>,

        /// Upload chunk size, for example 50M or 1G.
        #[arg(long = "chunk-size", default_value = "100MiB")]
        chunk_size: String,

        /// Skip post-upload size verification.
        #[arg(long = "no-verify")]
        no_verify: bool,

        /// Idle timeout in seconds while uploading a chunk.
        #[arg(long = "timeout")]
        timeout: Option<u64>,

        /// Retry count for retryable network/server failures.
        #[arg(long = "retries")]
        retries: Option<u32>,

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
    /// Inspect or clear local history.
    History {
        #[command(subcommand)]
        command: HistoryCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommands {
    /// List recent history entries.
    List {
        /// Print one JSON object.
        #[arg(long = "json")]
        json: bool,

        /// Maximum number of entries to show.
        #[arg(short = 'n', long = "limit", default_value_t = 20)]
        limit: usize,
    },
    /// Clear the history file.
    Clear,
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
    let Cli {
        verbose: _,
        config: config_path,
        no_config,
        history,
        no_history,
        command,
    } = cli;
    let config = config::load(config::LoadOptions {
        path: config_path.as_deref(),
        no_config,
    })?;
    let history_settings = history::settings(&config, history_override(history, no_history))?;

    match command {
        Commands::Download {
            url,
            output,
            key,
            force,
            no_resume,
            threads,
            timeout,
            retries,
            user_agent,
            dump_page,
            json,
            quiet,
        } => {
            let page_url = url.clone();
            let output = resolve_download_output(&config, output)?;
            let threads = config.resolve_download_threads(threads)?;
            let result = download::download(download::DownloadOptions {
                url,
                output,
                key,
                force,
                no_resume,
                threads,
                timeout: Duration::from_secs(config.resolve_timeout_secs(timeout)),
                retries: config.resolve_retries(retries),
                user_agent: config.resolve_user_agent(user_agent),
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
                        record_download_history(
                            &history_settings,
                            page_url,
                            &report,
                            code.to_string(),
                        );
                        Ok(RunOutcome::Failure(code))
                    } else {
                        record_download_history(
                            &history_settings,
                            page_url,
                            &report,
                            "ok".to_owned(),
                        );
                        Ok(RunOutcome::Success)
                    }
                }
                Err(error) if json => {
                    let code = error.exit_code();
                    jsonout::print_error(&error)?;
                    record_download_error_history(&history_settings, page_url, code);
                    Ok(RunOutcome::Failure(code))
                }
                Err(error) => {
                    let code = error.exit_code();
                    record_download_error_history(&history_settings, page_url, code);
                    Err(error)
                }
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
                timeout: Duration::from_secs(config.resolve_timeout_secs(timeout)),
                retries: config.resolve_retries(retries),
                user_agent: config.resolve_user_agent(user_agent),
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
            let input_file = file.clone();
            let result = upload::upload(upload::UploadOptions {
                file,
                lifetime: config.resolve_lifetime(lifetime),
                chunk_size: upload::parse_chunk_size(&chunk_size)?,
                verify: !no_verify,
                timeout: Duration::from_secs(config.resolve_timeout_secs(timeout)),
                retries: config.resolve_retries(retries),
                user_agent: config.resolve_user_agent(user_agent),
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
                    record_upload_history(&history_settings, &input_file, &report, "ok".to_owned());
                    Ok(RunOutcome::Success)
                }
                Err(error) if json => {
                    let code = error.exit_code();
                    jsonout::print_error(&error)?;
                    record_upload_error_history(&history_settings, &input_file, code);
                    Ok(RunOutcome::Failure(code))
                }
                Err(error) => {
                    let code = error.exit_code();
                    record_upload_error_history(&history_settings, &input_file, code);
                    Err(error)
                }
            }
        }
        Commands::History { command } => match command {
            HistoryCommands::List { json, limit } => {
                let records = history::latest(history::read(&history_settings.path)?, limit);
                if json {
                    jsonout::print_json(&HistoryListJson {
                        status: "ok",
                        entries: &records,
                    })?;
                } else {
                    print_human_history(&records);
                }
                Ok(RunOutcome::Success)
            }
            HistoryCommands::Clear => {
                history::clear(&history_settings.path)?;
                println!("history cleared");
                Ok(RunOutcome::Success)
            }
        },
    }
}

#[derive(Debug, Serialize)]
struct HistoryListJson<'a> {
    status: &'static str,
    entries: &'a [HistoryRecord],
}

fn history_override(history: bool, no_history: bool) -> HistoryOverride {
    if history {
        HistoryOverride::Enable
    } else if no_history {
        HistoryOverride::Disable
    } else {
        HistoryOverride::Auto
    }
}

fn resolve_download_output(
    config: &config::AppConfig,
    cli_output: Option<PathBuf>,
) -> Result<Option<PathBuf>, GfileError> {
    if cli_output.is_some() {
        return Ok(cli_output);
    }
    let Some(path) = config.download.dir.clone() else {
        return Ok(None);
    };
    std::fs::create_dir_all(&path).map_err(|source| GfileError::Io {
        source,
        path: path.clone(),
        op: IoOp::Create,
    })?;
    Ok(Some(path))
}

fn record_download_history(
    settings: &history::HistorySettings,
    page_url: String,
    report: &download::DownloadReport,
    result: String,
) {
    let record = HistoryRecord::download(
        page_url,
        report.files.iter().map(|file| file.name.clone()).collect(),
        total_download_bytes(report),
        result,
    );
    history::append(settings, &record);
}

fn record_download_error_history(
    settings: &history::HistorySettings,
    page_url: String,
    exit_code: u8,
) {
    let record = HistoryRecord::download(page_url, Vec::new(), None, exit_code.to_string());
    history::append(settings, &record);
}

fn record_upload_history(
    settings: &history::HistorySettings,
    input_file: &Path,
    report: &upload::UploadReport,
    result: String,
) {
    let filename = report
        .remote_filename
        .clone()
        .or_else(|| local_file_name(input_file))
        .into_iter()
        .collect();
    let delete_key = settings
        .store_delete_keys
        .then(|| report.delkey.clone())
        .flatten();
    let record = HistoryRecord::upload(
        report.url.clone(),
        filename,
        Some(report.bytes),
        result,
        delete_key,
    );
    history::append(settings, &record);
}

fn record_upload_error_history(
    settings: &history::HistorySettings,
    input_file: &Path,
    exit_code: u8,
) {
    let record = HistoryRecord::upload(
        String::new(),
        local_file_name(input_file).into_iter().collect(),
        local_file_size(input_file),
        exit_code.to_string(),
        None,
    );
    history::append(settings, &record);
}

fn total_download_bytes(report: &download::DownloadReport) -> Option<u64> {
    let mut saw_bytes = false;
    let total = report
        .files
        .iter()
        .filter_map(|file| {
            let bytes = file.bytes?;
            saw_bytes = true;
            Some(bytes)
        })
        .sum();
    saw_bytes.then_some(total)
}

fn local_file_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
}

fn local_file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
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

fn print_human_history(records: &[HistoryRecord]) {
    println!("timestamp\toperation\tresult\tbytes\tfiles\turl");
    for record in records {
        let operation = match record.operation {
            history::HistoryOperation::Download => "download",
            history::HistoryOperation::Upload => "upload",
        };
        let bytes = record
            .bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let files = if record.files.is_empty() {
            "-".to_owned()
        } else {
            record.files.join(",")
        };
        let url = if record.page_url.is_empty() {
            "-"
        } else {
            &record.page_url
        };
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            record.timestamp, operation, record.result, bytes, files, url
        );
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
