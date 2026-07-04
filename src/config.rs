// SPDX-License-Identifier: GPL-3.0-only

use std::{
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::Deserialize;

use crate::{
    download,
    error::{GfileError, IoOp},
    upload,
};

pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_RETRIES: u32 = 3;
pub const DEFAULT_UPLOAD_LIFETIME: u16 = 100;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub download: DownloadConfig,
    pub upload: UploadConfig,
    pub network: NetworkConfig,
    pub history: HistoryConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloadConfig {
    pub dir: Option<PathBuf>,
    pub threads: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadConfig {
    pub lifetime: Option<u16>,
    pub threads: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub timeout: Option<u64>,
    pub retries: Option<u32>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    pub enabled: Option<bool>,
    pub store_delete_keys: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
pub struct LoadOptions<'a> {
    pub path: Option<&'a Path>,
    pub no_config: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigInspection {
    pub path: Option<PathBuf>,
    pub exists: bool,
    pub no_config: bool,
    pub config: AppConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigValueSource {
    File,
    Default,
    Unset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitAnswers {
    pub download_dir: Option<String>,
    pub download_threads: u8,
    pub upload_lifetime: u16,
    pub upload_threads: u8,
    pub history_enabled: bool,
    pub history_store_delete_keys: Option<bool>,
}

impl AppConfig {
    pub fn resolve_download_output(&self, cli_output: Option<PathBuf>) -> Option<PathBuf> {
        cli_output.or_else(|| self.download.dir.clone())
    }

    pub fn resolve_download_threads(&self, cli_threads: Option<u8>) -> Result<u8, GfileError> {
        download::validate_threads(
            cli_threads
                .or(self.download.threads)
                .unwrap_or(download::DEFAULT_DOWNLOAD_THREADS),
        )
    }

    pub fn resolve_timeout_secs(&self, cli_timeout: Option<u64>) -> u64 {
        cli_timeout
            .or(self.network.timeout)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
    }

    pub fn resolve_retries(&self, cli_retries: Option<u32>) -> u32 {
        cli_retries
            .or(self.network.retries)
            .unwrap_or(DEFAULT_RETRIES)
    }

    pub fn resolve_user_agent(&self, cli_user_agent: Option<String>) -> Option<String> {
        cli_user_agent.or_else(|| self.network.user_agent.clone())
    }

    pub fn resolve_lifetime(&self, cli_lifetime: Option<u16>) -> u16 {
        cli_lifetime
            .or(self.upload.lifetime)
            .unwrap_or(DEFAULT_UPLOAD_LIFETIME)
    }

    pub fn resolve_upload_threads(&self, cli_threads: Option<u8>) -> Result<u8, GfileError> {
        upload::validate_threads(
            cli_threads
                .or(self.upload.threads)
                .unwrap_or(upload::DEFAULT_UPLOAD_THREADS),
        )
    }
}

pub fn load(options: LoadOptions<'_>) -> Result<AppConfig, GfileError> {
    if options.no_config {
        return Ok(AppConfig::default());
    }

    let Some(path) = options
        .path
        .map(Path::to_owned)
        .or_else(default_config_path)
    else {
        return Ok(AppConfig::default());
    };

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(AppConfig::default());
        }
        Err(source) => return Err(io_error(source, &path, IoOp::Read)),
    };

    parse_text(&text, &path)
}

pub fn inspect(options: LoadOptions<'_>) -> Result<ConfigInspection, GfileError> {
    if options.no_config {
        return Ok(ConfigInspection {
            path: None,
            exists: false,
            no_config: true,
            config: AppConfig::default(),
        });
    }

    let Some(path) = options
        .path
        .map(Path::to_owned)
        .or_else(default_config_path)
    else {
        return Ok(ConfigInspection {
            path: None,
            exists: false,
            no_config: false,
            config: AppConfig::default(),
        });
    };

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ConfigInspection {
                path: Some(path),
                exists: false,
                no_config: false,
                config: AppConfig::default(),
            });
        }
        Err(source) => return Err(io_error(source, &path, IoOp::Read)),
    };

    Ok(ConfigInspection {
        path: Some(path.clone()),
        exists: true,
        no_config: false,
        config: parse_text(&text, &path)?,
    })
}

pub fn resolved_config_path(path: Option<&Path>) -> Result<PathBuf, GfileError> {
    path.map(Path::to_owned)
        .or_else(default_config_path)
        .ok_or_else(|| GfileError::Usage {
            message: "could not determine platform config directory; pass --config <path>"
                .to_owned(),
        })
}

pub fn default_config_path() -> Option<PathBuf> {
    Some(
        BaseDirs::new()?
            .config_dir()
            .join("rgfile")
            .join("config.toml"),
    )
}

impl ConfigInspection {
    pub fn source_download_dir(&self) -> ConfigValueSource {
        optional_source(self.config.download.dir.is_some())
    }

    pub fn source_download_threads(&self) -> ConfigValueSource {
        defaulted_source(self.config.download.threads.is_some())
    }

    pub fn source_upload_lifetime(&self) -> ConfigValueSource {
        defaulted_source(self.config.upload.lifetime.is_some())
    }

    pub fn source_upload_threads(&self) -> ConfigValueSource {
        defaulted_source(self.config.upload.threads.is_some())
    }

    pub fn source_network_timeout(&self) -> ConfigValueSource {
        defaulted_source(self.config.network.timeout.is_some())
    }

    pub fn source_network_retries(&self) -> ConfigValueSource {
        defaulted_source(self.config.network.retries.is_some())
    }

    pub fn source_network_user_agent(&self) -> ConfigValueSource {
        optional_source(self.config.network.user_agent.is_some())
    }

    pub fn source_history_enabled(&self) -> ConfigValueSource {
        defaulted_source(self.config.history.enabled.is_some())
    }

    pub fn source_history_store_delete_keys(&self) -> ConfigValueSource {
        defaulted_source(self.config.history.store_delete_keys.is_some())
    }
}

impl ConfigValueSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Default => "default",
            Self::Unset => "unset",
        }
    }
}

fn defaulted_source(is_file_value: bool) -> ConfigValueSource {
    if is_file_value {
        ConfigValueSource::File
    } else {
        ConfigValueSource::Default
    }
}

fn optional_source(is_file_value: bool) -> ConfigValueSource {
    if is_file_value {
        ConfigValueSource::File
    } else {
        ConfigValueSource::Unset
    }
}

pub fn default_config_template() -> String {
    format!(
        concat!(
            "# rgfile configuration\n",
            "# Uncomment keys you want to set. CLI flags override these values.\n",
            "\n",
            "[download]\n",
            "# dir = \"/absolute/path/to/downloads\"\n",
            "# threads = {}\n",
            "\n",
            "[upload]\n",
            "# lifetime = {}\n",
            "# threads = {}\n",
            "\n",
            "[network]\n",
            "# timeout = {}\n",
            "# retries = {}\n",
            "# user_agent = \"rgfile/{}\"\n",
            "\n",
            "[history]\n",
            "# enabled = false\n",
            "# store_delete_keys = false\n",
        ),
        download::DEFAULT_DOWNLOAD_THREADS,
        DEFAULT_UPLOAD_LIFETIME,
        upload::DEFAULT_UPLOAD_THREADS,
        DEFAULT_TIMEOUT_SECS,
        DEFAULT_RETRIES,
        env!("CARGO_PKG_VERSION")
    )
}

pub fn run_init_wizard(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<String, GfileError> {
    let answers = prompt_init_answers(reader, writer)?;
    Ok(render_init_answers(&answers))
}

pub fn confirm_overwrite(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    path: &Path,
) -> Result<bool, GfileError> {
    prompt_bool(
        reader,
        writer,
        &format!("Config file {} already exists. Overwrite?", path.display()),
        false,
    )
}

fn prompt_init_answers(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<InitAnswers, GfileError> {
    let download_dir = prompt_optional_text(
        reader,
        writer,
        "download.dir [unset; use an absolute path, empty keeps current directory]",
    )?;
    let download_threads = prompt_download_threads(reader, writer)?;
    let upload_lifetime = prompt_upload_lifetime(reader, writer)?;
    let upload_threads = prompt_upload_threads(reader, writer)?;
    let history_enabled = prompt_bool(reader, writer, "history.enabled", false)?;
    let history_store_delete_keys = if history_enabled {
        let store = prompt_bool(
            reader,
            writer,
            "history.store_delete_keys [stores upload delete keys in plaintext]",
            false,
        )?;
        Some(store)
    } else {
        None
    };

    Ok(InitAnswers {
        download_dir,
        download_threads,
        upload_lifetime,
        upload_threads,
        history_enabled,
        history_store_delete_keys,
    })
}

fn prompt_optional_text(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    label: &str,
) -> Result<Option<String>, GfileError> {
    write_prompt(writer, &format!("{label}: "))?;
    let line = read_prompt_line(reader)?;
    if line.is_empty() {
        Ok(None)
    } else {
        Ok(Some(line))
    }
}

fn prompt_download_threads(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<u8, GfileError> {
    loop {
        write_prompt(
            writer,
            &format!(
                "download.threads [{}]: ",
                download::DEFAULT_DOWNLOAD_THREADS
            ),
        )?;
        let line = read_prompt_line(reader)?;
        if line.trim().is_empty() {
            return Ok(download::DEFAULT_DOWNLOAD_THREADS);
        }
        match line
            .trim()
            .parse::<u8>()
            .ok()
            .and_then(|value| download::validate_threads(value).ok())
        {
            Some(value) => return Ok(value),
            None => write_invalid(writer, "download threads must be between 1 and 16")?,
        }
    }
}

fn prompt_upload_lifetime(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<u16, GfileError> {
    loop {
        write_prompt(
            writer,
            &format!("upload.lifetime [{}]: ", DEFAULT_UPLOAD_LIFETIME),
        )?;
        let line = read_prompt_line(reader)?;
        if line.trim().is_empty() {
            return Ok(DEFAULT_UPLOAD_LIFETIME);
        }
        match line
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|value| upload::validate_lifetime(*value).is_ok())
        {
            Some(value) => return Ok(value),
            None => write_invalid(
                writer,
                "lifetime must be one of 3, 5, 7, 14, 30, 60, or 100 days",
            )?,
        }
    }
}

fn prompt_upload_threads(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<u8, GfileError> {
    loop {
        write_prompt(
            writer,
            &format!("upload.threads [{}]: ", upload::DEFAULT_UPLOAD_THREADS),
        )?;
        let line = read_prompt_line(reader)?;
        if line.trim().is_empty() {
            return Ok(upload::DEFAULT_UPLOAD_THREADS);
        }
        match line
            .trim()
            .parse::<u8>()
            .ok()
            .and_then(|value| upload::validate_threads(value).ok())
        {
            Some(value) => return Ok(value),
            None => write_invalid(writer, "upload threads must be between 1 and 16")?,
        }
    }
}

fn prompt_bool(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    label: &str,
    default: bool,
) -> Result<bool, GfileError> {
    let default_label = if default { "Y/n" } else { "y/N" };
    loop {
        write_prompt(writer, &format!("{label} [{default_label}]: "))?;
        let line = read_prompt_line(reader)?;
        let value = line.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Ok(default);
        }
        match value.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => write_invalid(writer, "answer y or n")?,
        }
    }
}

fn read_prompt_line(reader: &mut impl BufRead) -> Result<String, GfileError> {
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|source| GfileError::Io {
            source,
            path: PathBuf::from("stdin"),
            op: IoOp::Read,
        })?;
    if read == 0 {
        return Err(GfileError::Usage {
            message: "config init aborted before all answers were provided".to_owned(),
        });
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(line)
}

fn write_prompt(writer: &mut impl Write, text: &str) -> Result<(), GfileError> {
    writer
        .write_all(text.as_bytes())
        .map_err(|source| GfileError::Io {
            source,
            path: PathBuf::from("stderr"),
            op: IoOp::Write,
        })?;
    writer.flush().map_err(|source| GfileError::Io {
        source,
        path: PathBuf::from("stderr"),
        op: IoOp::Write,
    })
}

fn write_invalid(writer: &mut impl Write, text: &str) -> Result<(), GfileError> {
    writeln!(writer, "Invalid value: {text}").map_err(|source| GfileError::Io {
        source,
        path: PathBuf::from("stderr"),
        op: IoOp::Write,
    })
}

fn render_init_answers(answers: &InitAnswers) -> String {
    let mut output = String::new();
    output.push_str("# rgfile configuration generated by `rgfile config init`\n");
    output.push_str("# CLI flags override these values.\n\n");
    output.push_str("[download]\n");
    if let Some(dir) = &answers.download_dir {
        output.push_str("dir = ");
        output.push_str(&toml_string(dir));
        output.push('\n');
    } else {
        output.push_str("# dir = \"/absolute/path/to/downloads\"\n");
    }
    output.push_str(&format!("threads = {}\n\n", answers.download_threads));

    output.push_str("[upload]\n");
    output.push_str(&format!("lifetime = {}\n", answers.upload_lifetime));
    output.push_str(&format!("threads = {}\n\n", answers.upload_threads));

    output.push_str("[network]\n");
    output.push_str(&format!("# timeout = {}\n", DEFAULT_TIMEOUT_SECS));
    output.push_str(&format!("# retries = {}\n", DEFAULT_RETRIES));
    output.push_str(&format!(
        "# user_agent = \"rgfile/{}\"\n\n",
        env!("CARGO_PKG_VERSION")
    ));

    output.push_str("[history]\n");
    output.push_str(&format!("enabled = {}\n", answers.history_enabled));
    if let Some(store) = answers.history_store_delete_keys {
        output.push_str(&format!("store_delete_keys = {store}\n"));
    } else {
        output.push_str("# store_delete_keys = false\n");
    }
    output
}

fn toml_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\u{08}' => output.push_str("\\b"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\u{0c}' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            ch if ch.is_control() => output.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn parse_text(text: &str, path: &Path) -> Result<AppConfig, GfileError> {
    let config: AppConfig = toml::from_str(text).map_err(|error| parse_error(error, text, path))?;
    validate(&config)?;
    Ok(config)
}

fn validate(config: &AppConfig) -> Result<(), GfileError> {
    if let Some(lifetime) = config.upload.lifetime {
        upload::validate_lifetime(lifetime)?;
    }
    if let Some(threads) = config.upload.threads {
        upload::validate_threads(threads)?;
    }
    if let Some(threads) = config.download.threads {
        download::validate_threads(threads)?;
    }
    Ok(())
}

fn parse_error(error: toml::de::Error, text: &str, path: &Path) -> GfileError {
    let line = error
        .span()
        .map(|span| line_number(text, span.start))
        .unwrap_or(1);
    GfileError::Usage {
        message: format!(
            "failed to parse config {} at line {line}: {error}",
            path.display()
        ),
    }
}

fn line_number(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn io_error(source: io::Error, path: &Path, op: IoOp) -> GfileError {
    GfileError::Io {
        source,
        path: path.to_owned(),
        op,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn missing_config_uses_defaults() {
        let temp = tempfile::TempDir::new().unwrap();
        let config = load(LoadOptions {
            path: Some(&temp.path().join("missing.toml")),
            no_config: false,
        })
        .unwrap();

        assert_eq!(config.resolve_timeout_secs(None), DEFAULT_TIMEOUT_SECS);
        assert_eq!(config.resolve_retries(None), DEFAULT_RETRIES);
        assert_eq!(
            config.resolve_download_threads(None).unwrap(),
            download::DEFAULT_DOWNLOAD_THREADS
        );
        assert_eq!(
            config.resolve_upload_threads(None).unwrap(),
            upload::DEFAULT_UPLOAD_THREADS
        );
        assert_eq!(config.resolve_lifetime(None), DEFAULT_UPLOAD_LIFETIME);
    }

    #[test]
    fn cli_values_override_config_values() {
        let temp = tempfile::TempDir::new().unwrap();
        let output = temp.path().join("from-config");
        let config = AppConfig {
            download: DownloadConfig {
                dir: Some(output.clone()),
                threads: Some(3),
            },
            upload: UploadConfig {
                lifetime: Some(7),
                threads: Some(4),
            },
            network: NetworkConfig {
                timeout: Some(9),
                retries: Some(1),
                user_agent: Some("from-config".to_owned()),
            },
            history: HistoryConfig::default(),
        };

        assert_eq!(
            config.resolve_download_output(Some(temp.path().join("from-cli"))),
            Some(temp.path().join("from-cli"))
        );
        assert_eq!(config.resolve_download_output(None), Some(output));
        assert_eq!(config.resolve_download_threads(Some(2)).unwrap(), 2);
        assert_eq!(config.resolve_download_threads(None).unwrap(), 3);
        assert_eq!(config.resolve_timeout_secs(Some(8)), 8);
        assert_eq!(config.resolve_timeout_secs(None), 9);
        assert_eq!(config.resolve_retries(Some(4)), 4);
        assert_eq!(config.resolve_retries(None), 1);
        assert_eq!(
            config.resolve_user_agent(Some("from-cli".to_owned())),
            Some("from-cli".to_owned())
        );
        assert_eq!(
            config.resolve_user_agent(None),
            Some("from-config".to_owned())
        );
        assert_eq!(config.resolve_lifetime(Some(5)), 5);
        assert_eq!(config.resolve_lifetime(None), 7);
        assert_eq!(config.resolve_upload_threads(Some(2)).unwrap(), 2);
        assert_eq!(config.resolve_upload_threads(None).unwrap(), 4);
    }

    #[test]
    fn parse_error_reports_line_number() {
        let error = parse_text("[network]\ntimeout =\n", Path::new("config.toml"))
            .expect_err("invalid TOML should fail");

        let GfileError::Usage { message } = error else {
            panic!("unexpected error");
        };
        assert!(message.contains("line 2"), "{message}");
    }

    #[test]
    fn invalid_config_lifetime_is_usage_error() {
        let error = parse_text("[upload]\nlifetime = 4\n", Path::new("config.toml"))
            .expect_err("unsupported lifetime should fail");

        assert_eq!(error.exit_code(), 2);
        assert!(error.user_message().contains("lifetime must be one of"));
    }

    #[test]
    fn invalid_download_threads_is_usage_error() {
        let error = parse_text("[download]\nthreads = 17\n", Path::new("config.toml"))
            .expect_err("unsupported thread count should fail");

        assert_eq!(error.exit_code(), 2);
        assert!(
            error
                .user_message()
                .contains("download threads must be between 1 and 16")
        );
    }

    #[test]
    fn invalid_upload_threads_is_usage_error() {
        let error = parse_text("[upload]\nthreads = 0\n", Path::new("config.toml"))
            .expect_err("unsupported upload thread count should fail");

        assert_eq!(error.exit_code(), 2);
        assert!(
            error
                .user_message()
                .contains("upload threads must be between 1 and 16")
        );
    }

    #[test]
    fn default_config_template_parses_as_default_config() {
        let text = default_config_template();
        let config = parse_text(&text, Path::new("config.toml")).unwrap();

        assert_eq!(config, AppConfig::default());
    }

    #[test]
    fn init_wizard_accepts_all_defaults() {
        let mut input = Cursor::new(b"\n\n\n\n\n".as_slice());
        let mut output = Vec::new();

        let text = run_init_wizard(&mut input, &mut output).unwrap();
        let config = parse_text(&text, Path::new("config.toml")).unwrap();

        assert_eq!(
            config,
            AppConfig {
                download: DownloadConfig {
                    dir: None,
                    threads: Some(1),
                },
                upload: UploadConfig {
                    lifetime: Some(100),
                    threads: Some(1),
                },
                network: NetworkConfig::default(),
                history: HistoryConfig {
                    enabled: Some(false),
                    store_delete_keys: None,
                },
            }
        );
        let prompt = String::from_utf8(output).unwrap();
        assert!(prompt.contains("download.dir"));
        assert!(prompt.contains("history.enabled"));
    }

    #[test]
    fn init_wizard_accepts_custom_values_and_history_delete_key_opt_in() {
        let mut input = Cursor::new(b"/tmp/rgfile downloads\n4\n7\n3\ny\ny\n".as_slice());
        let mut output = Vec::new();

        let text = run_init_wizard(&mut input, &mut output).unwrap();
        let config = parse_text(&text, Path::new("config.toml")).unwrap();

        assert_eq!(
            config,
            AppConfig {
                download: DownloadConfig {
                    dir: Some(PathBuf::from("/tmp/rgfile downloads")),
                    threads: Some(4),
                },
                upload: UploadConfig {
                    lifetime: Some(7),
                    threads: Some(3),
                },
                network: NetworkConfig::default(),
                history: HistoryConfig {
                    enabled: Some(true),
                    store_delete_keys: Some(true),
                },
            }
        );
        assert!(text.contains("dir = \"/tmp/rgfile downloads\""));
    }

    #[test]
    fn init_wizard_reprompts_invalid_values() {
        let mut input = Cursor::new(b"\n0\n2\n4\n14\n17\n5\nmaybe\nn\n".as_slice());
        let mut output = Vec::new();

        let text = run_init_wizard(&mut input, &mut output).unwrap();
        let config = parse_text(&text, Path::new("config.toml")).unwrap();

        assert_eq!(config.download.threads, Some(2));
        assert_eq!(config.upload.lifetime, Some(14));
        assert_eq!(config.upload.threads, Some(5));
        assert_eq!(config.history.enabled, Some(false));
        let prompt = String::from_utf8(output).unwrap();
        assert!(prompt.contains("download threads must be between 1 and 16"));
        assert!(prompt.contains("lifetime must be one of"));
        assert!(prompt.contains("upload threads must be between 1 and 16"));
        assert!(prompt.contains("answer y or n"));
    }

    #[test]
    fn init_wizard_eof_aborts_before_writing_config() {
        let mut input = Cursor::new(b"".as_slice());
        let mut output = Vec::new();

        let error = run_init_wizard(&mut input, &mut output).unwrap_err();

        assert_eq!(error.exit_code(), 2);
        assert!(error.user_message().contains("aborted"));
    }

    #[test]
    fn confirm_overwrite_defaults_to_no_and_accepts_yes() {
        let mut no_input = Cursor::new(b"\n".as_slice());
        let mut no_output = Vec::new();
        assert!(
            !confirm_overwrite(&mut no_input, &mut no_output, Path::new("config.toml")).unwrap()
        );

        let mut yes_input = Cursor::new(b"yes\n".as_slice());
        let mut yes_output = Vec::new();
        assert!(
            confirm_overwrite(&mut yes_input, &mut yes_output, Path::new("config.toml")).unwrap()
        );
    }
}
