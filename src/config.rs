// SPDX-License-Identifier: GPL-3.0-only

use std::{
    fs, io,
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

pub fn default_config_path() -> Option<PathBuf> {
    Some(
        BaseDirs::new()?
            .config_dir()
            .join("rgfile")
            .join("config.toml"),
    )
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
            upload: UploadConfig { lifetime: Some(7) },
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
}
