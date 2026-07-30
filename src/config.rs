// SPDX-License-Identifier: MIT

use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, BufRead, Read as _, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use directories::BaseDirs;
use serde::Deserialize;

use crate::{
    download,
    error::{GfileError, IoOp, io_error},
    fsutil, http,
    naming::escape_terminal_text,
    upload,
};

pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_RETRIES: u32 = 3;
pub const DEFAULT_UPLOAD_LIFETIME: u16 = 100;
pub const MAX_TIMEOUT_SECS: u64 = 86_400;
pub const MAX_RETRIES: u32 = 20;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

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

    pub fn resolve_timeout_secs(&self, cli_timeout: Option<u64>) -> Result<u64, GfileError> {
        let timeout = cli_timeout
            .or(self.network.timeout)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        validate_timeout(timeout)?;
        Ok(timeout)
    }

    pub fn resolve_retries(&self, cli_retries: Option<u32>) -> Result<u32, GfileError> {
        let retries = cli_retries
            .or(self.network.retries)
            .unwrap_or(DEFAULT_RETRIES);
        validate_retries(retries)?;
        Ok(retries)
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

    let explicit_path = options.path.is_some();
    let Some(path) = options
        .path
        .map(Path::to_owned)
        .or_else(default_config_path)
    else {
        return Ok(AppConfig::default());
    };

    let text = match read_config_text(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !explicit_path => {
            return Ok(AppConfig::default());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(GfileError::Usage {
                message: format!(
                    "explicit config file does not exist: {}; fix --config or remove the option",
                    path.display()
                ),
            });
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

    let text = match read_config_text(&path) {
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

fn read_config_text(path: &Path) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration file is too large or is not a regular file",
        ));
    }
    let file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration file exceeds the size limit",
        ));
    }
    String::from_utf8(bytes).map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))
}

pub fn write_config_file(path: &Path, text: &str, overwrite: bool) -> Result<(), GfileError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| io_error(source, parent, IoOp::Create))?;

    let file_name = path.file_name().ok_or_else(|| GfileError::Usage {
        message: format!("config path must name a file: {}", path.display()),
    })?;
    let mut temp_name = OsString::from(file_name);
    temp_name.push(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    let temp_path = path.with_file_name(temp_name);

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        let mut file = options
            .open(&temp_path)
            .map_err(|source| io_error(source, &temp_path, IoOp::Create))?;
        restrict_config_permissions(&file)
            .map_err(|source| io_error(source, &temp_path, IoOp::Write))?;
        file.write_all(text.as_bytes())
            .map_err(|source| io_error(source, &temp_path, IoOp::Write))?;
        file.sync_all()
            .map_err(|source| io_error(source, &temp_path, IoOp::Write))?;
        drop(file);

        install_config_file(&temp_path, path, overwrite)
            .map_err(|source| io_error(source, path, IoOp::Write))?;
        sync_config_parent(parent).map_err(|source| io_error(source, parent, IoOp::Write))
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn install_config_file(temp_path: &Path, path: &Path, overwrite: bool) -> io::Result<()> {
    if overwrite {
        return fsutil::replace_file(temp_path, path);
    }

    fsutil::move_file_noreplace(temp_path, path)
}

#[cfg(unix)]
fn restrict_config_permissions(file: &fs::File) -> io::Result<()> {
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_config_permissions(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_config_parent(parent: &Path) -> io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_config_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
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
        &format!(
            "Config file {} already exists. Overwrite?",
            escape_terminal_text(&path.to_string_lossy())
        ),
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
    if let Some(timeout) = config.network.timeout {
        validate_timeout(timeout)?;
    }
    if let Some(retries) = config.network.retries {
        validate_retries(retries)?;
    }
    if let Some(user_agent) = config.network.user_agent.as_deref() {
        http::validate_user_agent(user_agent)?;
    }
    Ok(())
}

fn validate_timeout(timeout: u64) -> Result<(), GfileError> {
    if (1..=MAX_TIMEOUT_SECS).contains(&timeout) {
        Ok(())
    } else {
        Err(GfileError::Usage {
            message: format!("network timeout must be between 1 and {MAX_TIMEOUT_SECS} seconds"),
        })
    }
}

fn validate_retries(retries: u32) -> Result<(), GfileError> {
    if retries <= MAX_RETRIES {
        Ok(())
    } else {
        Err(GfileError::Usage {
            message: format!("network retries must be between 0 and {MAX_RETRIES}"),
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn config_write_is_atomic_and_requires_explicit_overwrite() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("nested").join("config.toml");

        write_config_file(&path, "first", false).unwrap();
        let error = write_config_file(&path, "second", false).unwrap_err();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        assert!(matches!(error, GfileError::Io { .. }));

        write_config_file(&path, "second", true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_write_restricts_permissions_to_owner() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("config.toml");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o666);
        options.open(&path).unwrap();

        write_config_file(&path, "[history]\nenabled = true\n", true).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let error = load(LoadOptions {
            path: Some(&temp.path().join("missing.toml")),
            no_config: false,
        })
        .unwrap_err();

        assert_eq!(error.exit_code(), 2);
        assert!(error.user_message().contains("does not exist"));
    }

    #[test]
    fn no_config_uses_defaults() {
        let config = load(LoadOptions {
            path: None,
            no_config: true,
        })
        .unwrap();

        assert_eq!(
            config.resolve_timeout_secs(None).unwrap(),
            DEFAULT_TIMEOUT_SECS
        );
        assert_eq!(config.resolve_retries(None).unwrap(), DEFAULT_RETRIES);
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
        assert_eq!(config.resolve_timeout_secs(Some(8)).unwrap(), 8);
        assert_eq!(config.resolve_timeout_secs(None).unwrap(), 9);
        assert_eq!(config.resolve_retries(Some(4)).unwrap(), 4);
        assert_eq!(config.resolve_retries(None).unwrap(), 1);
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
    fn invalid_network_limits_are_usage_errors() {
        for text in [
            "[network]\ntimeout = 0\n",
            "[network]\ntimeout = 86401\n",
            "[network]\nretries = 21\n",
        ] {
            let error = parse_text(text, Path::new("config.toml")).unwrap_err();
            assert_eq!(error.exit_code(), 2);
        }
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
