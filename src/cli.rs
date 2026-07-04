// SPDX-License-Identifier: GPL-3.0-only

use std::{
    env,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use serde::Serialize;
use tracing_subscriber::EnvFilter;

use crate::{
    config, download,
    error::{GfileError, IoOp},
    history::{self, HistoryOverride, HistoryRecord},
    info, jsonout, self_update, upload,
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

        /// Select files by 1-based index list/ranges, for example 1,3-5.
        #[arg(long = "select", value_name = "SPEC")]
        select: Option<String>,

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

        /// Upload read-ahead chunk window (1-16; completion remains ordered).
        #[arg(long = "threads")]
        threads: Option<u8>,

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
    /// Inspect or create the rgfile configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Update this rgfile binary from the latest GitHub Release.
    SelfUpdate,
    /// Generate shell completion scripts.
    Completions {
        /// Shell to generate completions for.
        shell: Shell,
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

#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Print the configuration file path.
    Path,
    /// Show the effective configuration.
    Show {
        /// Print one JSON object.
        #[arg(long = "json")]
        json: bool,
    },
    /// Create a configuration file.
    Init {
        /// Write a commented defaults template without prompting.
        #[arg(long = "defaults")]
        defaults: bool,

        /// Overwrite an existing configuration file.
        #[arg(long = "force")]
        force: bool,
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
    let Cli {
        verbose: _,
        config: config_path,
        no_config,
        history,
        no_history,
        command,
    } = cli;
    match command {
        Commands::Config { command } => {
            run_config_command(command, config_path.as_deref(), no_config)
        }
        command => {
            let config = config::load(config::LoadOptions {
                path: config_path.as_deref(),
                no_config,
            })?;
            let history_settings =
                history::settings(&config, history_override(history, no_history))?;

            match command {
                Commands::Download {
                    url,
                    output,
                    key,
                    select,
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
                    let selection = select
                        .as_deref()
                        .map(download::FileSelection::parse)
                        .transpose()?;
                    let result = download::download(download::DownloadOptions {
                        url,
                        output,
                        key,
                        selection,
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
                    threads,
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
                        threads: config.resolve_upload_threads(threads)?,
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
                            record_upload_history(
                                &history_settings,
                                &input_file,
                                &report,
                                "ok".to_owned(),
                            );
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
                        let records =
                            history::latest(history::read(&history_settings.path)?, limit);
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
                Commands::SelfUpdate => {
                    match self_update::self_update(self_update::SelfUpdateOptions {
                        base_url: self_update_base_url(),
                        force: self_update_force(),
                    })
                    .await?
                    {
                        self_update::SelfUpdateReport::AlreadyUpToDate { version } => {
                            println!("rgfile {version} is already up to date");
                        }
                        self_update::SelfUpdateReport::Updated {
                            old_version,
                            new_version,
                            target,
                            path,
                        } => {
                            println!(
                                "updated rgfile {old_version} -> {new_version} ({target}) at {}",
                                path.display()
                            );
                        }
                    }
                    Ok(RunOutcome::Success)
                }
                Commands::Completions { shell } => {
                    let mut command = Cli::command();
                    clap_complete::generate(shell, &mut command, "rgfile", &mut io::stdout());
                    Ok(RunOutcome::Success)
                }
                Commands::Config { .. } => {
                    unreachable!("config command handled before config loading")
                }
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct HistoryListJson<'a> {
    status: &'static str,
    entries: &'a [HistoryRecord],
}

#[derive(Debug, Serialize)]
struct ConfigShowJson {
    status: &'static str,
    path: Option<String>,
    exists: bool,
    source: &'static str,
    values: ConfigValuesJson,
}

#[derive(Debug, Serialize)]
struct ConfigValuesJson {
    download: ConfigDownloadJson,
    upload: ConfigUploadJson,
    network: ConfigNetworkJson,
    history: ConfigHistoryJson,
}

#[derive(Debug, Serialize)]
struct ConfigDownloadJson {
    dir: ConfigValueJson<String>,
    threads: ConfigValueJson<u8>,
}

#[derive(Debug, Serialize)]
struct ConfigUploadJson {
    lifetime: ConfigValueJson<u16>,
    threads: ConfigValueJson<u8>,
}

#[derive(Debug, Serialize)]
struct ConfigNetworkJson {
    timeout: ConfigValueJson<u64>,
    retries: ConfigValueJson<u32>,
    user_agent: ConfigValueJson<String>,
}

#[derive(Debug, Serialize)]
struct ConfigHistoryJson {
    enabled: ConfigValueJson<bool>,
    store_delete_keys: ConfigValueJson<bool>,
}

#[derive(Debug, Serialize)]
struct ConfigValueJson<T: Serialize> {
    value: Option<T>,
    source: &'static str,
}

fn run_config_command(
    command: ConfigCommands,
    config_path: Option<&Path>,
    no_config: bool,
) -> Result<RunOutcome, GfileError> {
    match command {
        ConfigCommands::Path => {
            let path = config::resolved_config_path(config_path)?;
            println!("{}", path.display());
            if !path.exists() {
                eprintln!("file does not exist yet");
            }
            Ok(RunOutcome::Success)
        }
        ConfigCommands::Show { json } => {
            let inspection = config::inspect(config::LoadOptions {
                path: config_path,
                no_config,
            })?;
            if json {
                jsonout::print_json(&config_show_json(&inspection))?;
            } else {
                print_human_config_show(&inspection);
            }
            Ok(RunOutcome::Success)
        }
        ConfigCommands::Init { defaults, force } => {
            let path = config::resolved_config_path(config_path)?;
            let text = if defaults {
                if path.exists() && !force {
                    return Err(GfileError::Usage {
                        message: format!(
                            "config file already exists at {}; pass --force to overwrite",
                            path.display()
                        ),
                    });
                }
                config::default_config_template()
            } else {
                if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                    return Err(GfileError::Usage {
                        message:
                            "config init is interactive; use --defaults in non-interactive runs"
                                .to_owned(),
                    });
                }
                let mut stdin = io::stdin().lock();
                let mut stderr = io::stderr().lock();
                if path.exists()
                    && !force
                    && !config::confirm_overwrite(&mut stdin, &mut stderr, &path)?
                {
                    return Err(GfileError::Usage {
                        message: "config init aborted; existing file was not overwritten"
                            .to_owned(),
                    });
                }
                config::run_init_wizard(&mut stdin, &mut stderr)?
            };
            write_config_file(&path, &text)?;
            println!("{}", path.display());
            eprintln!("Wrote config. Run `rgfile config show` to inspect it.");
            Ok(RunOutcome::Success)
        }
    }
}

fn write_config_file(path: &Path, text: &str) -> Result<(), GfileError> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| GfileError::Io {
            source,
            path: parent.to_owned(),
            op: IoOp::Create,
        })?;
    }
    std::fs::write(path, text).map_err(|source| GfileError::Io {
        source,
        path: path.to_owned(),
        op: IoOp::Write,
    })
}

fn print_human_config_show(inspection: &config::ConfigInspection) {
    println!("source: {}", config_show_source_description(inspection));
    if let Some(path) = &inspection.path {
        println!("path: {}", path.display());
    }
    println!("exists: {}", inspection.exists);
    print_config_value(
        "download.dir",
        inspection
            .config
            .download
            .dir
            .as_ref()
            .map(|path| path.display().to_string()),
        inspection.source_download_dir(),
    );
    print_config_value(
        "download.threads",
        Some(inspection.config.resolve_download_threads(None).unwrap()),
        inspection.source_download_threads(),
    );
    print_config_value(
        "upload.lifetime",
        Some(inspection.config.resolve_lifetime(None)),
        inspection.source_upload_lifetime(),
    );
    print_config_value(
        "upload.threads",
        Some(inspection.config.resolve_upload_threads(None).unwrap()),
        inspection.source_upload_threads(),
    );
    print_config_value(
        "network.timeout",
        Some(inspection.config.resolve_timeout_secs(None)),
        inspection.source_network_timeout(),
    );
    print_config_value(
        "network.retries",
        Some(inspection.config.resolve_retries(None)),
        inspection.source_network_retries(),
    );
    print_config_value(
        "network.user_agent",
        inspection.config.resolve_user_agent(None),
        inspection.source_network_user_agent(),
    );
    print_config_value(
        "history.enabled",
        Some(inspection.config.history.enabled.unwrap_or(false)),
        inspection.source_history_enabled(),
    );
    print_config_value(
        "history.store_delete_keys",
        Some(inspection.config.history.store_delete_keys.unwrap_or(false)),
        inspection.source_history_store_delete_keys(),
    );
}

fn print_config_value<T: std::fmt::Display>(
    key: &str,
    value: Option<T>,
    source: config::ConfigValueSource,
) {
    match value {
        Some(value) => println!("{key} = {value} ({})", source.as_str()),
        None => println!("{key} = <unset> ({})", source.as_str()),
    }
}

fn config_show_json(inspection: &config::ConfigInspection) -> ConfigShowJson {
    ConfigShowJson {
        status: "ok",
        path: inspection
            .path
            .as_ref()
            .map(|path| path.display().to_string()),
        exists: inspection.exists,
        source: config_show_source(inspection),
        values: ConfigValuesJson {
            download: ConfigDownloadJson {
                dir: config_value(
                    inspection
                        .config
                        .download
                        .dir
                        .as_ref()
                        .map(|path| path.display().to_string()),
                    inspection.source_download_dir(),
                ),
                threads: config_value(
                    Some(inspection.config.resolve_download_threads(None).unwrap()),
                    inspection.source_download_threads(),
                ),
            },
            upload: ConfigUploadJson {
                lifetime: config_value(
                    Some(inspection.config.resolve_lifetime(None)),
                    inspection.source_upload_lifetime(),
                ),
                threads: config_value(
                    Some(inspection.config.resolve_upload_threads(None).unwrap()),
                    inspection.source_upload_threads(),
                ),
            },
            network: ConfigNetworkJson {
                timeout: config_value(
                    Some(inspection.config.resolve_timeout_secs(None)),
                    inspection.source_network_timeout(),
                ),
                retries: config_value(
                    Some(inspection.config.resolve_retries(None)),
                    inspection.source_network_retries(),
                ),
                user_agent: config_value(
                    inspection.config.resolve_user_agent(None),
                    inspection.source_network_user_agent(),
                ),
            },
            history: ConfigHistoryJson {
                enabled: config_value(
                    Some(inspection.config.history.enabled.unwrap_or(false)),
                    inspection.source_history_enabled(),
                ),
                store_delete_keys: config_value(
                    Some(inspection.config.history.store_delete_keys.unwrap_or(false)),
                    inspection.source_history_store_delete_keys(),
                ),
            },
        },
    }
}

fn config_value<T: Serialize>(
    value: Option<T>,
    source: config::ConfigValueSource,
) -> ConfigValueJson<T> {
    ConfigValueJson {
        value,
        source: source.as_str(),
    }
}

fn config_show_source(inspection: &config::ConfigInspection) -> &'static str {
    if inspection.no_config {
        "no_config"
    } else if inspection.exists {
        "file"
    } else {
        "default"
    }
}

fn config_show_source_description(inspection: &config::ConfigInspection) -> String {
    if inspection.no_config {
        "--no-config (using defaults)".to_owned()
    } else if inspection.exists {
        match &inspection.path {
            Some(path) => format!("loaded from {}", path.display()),
            None => "loaded from config file".to_owned(),
        }
    } else {
        match &inspection.path {
            Some(path) => format!(
                "no config file found at {} (using defaults)",
                path.display()
            ),
            None => "no config file found (using defaults)".to_owned(),
        }
    }
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
        println!(
            "[{}]\tdisplay_name (may be masked)\t{}",
            file.index, file.display_name
        );
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

fn self_update_base_url() -> Option<String> {
    env::var("RGFILE_TEST_UPDATE_BASE_URL").ok()
}

fn self_update_force() -> bool {
    env::var("RGFILE_TEST_FORCE_UPDATE").as_deref() == Ok("1")
}
