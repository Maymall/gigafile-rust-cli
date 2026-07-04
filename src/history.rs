// SPDX-License-Identifier: MIT

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::{
    config::AppConfig,
    error::{GfileError, IoOp, io_error},
    timeutil,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySettings {
    pub enabled: bool,
    pub store_delete_keys: bool,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryOverride {
    Auto,
    Enable,
    Disable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub timestamp: String,
    pub operation: HistoryOperation,
    pub page_url: String,
    pub files: Vec<String>,
    pub bytes: Option<u64>,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryOperation {
    Download,
    Upload,
    Delete,
}

impl HistoryRecord {
    pub fn download(
        page_url: String,
        files: Vec<String>,
        bytes: Option<u64>,
        result: String,
    ) -> Self {
        Self {
            timestamp: timeutil::now_utc_timestamp()
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned()),
            operation: HistoryOperation::Download,
            page_url,
            files,
            bytes,
            result,
            delete_key: None,
        }
    }

    pub fn upload(
        page_url: String,
        files: Vec<String>,
        bytes: Option<u64>,
        result: String,
        delete_key: Option<String>,
    ) -> Self {
        Self {
            timestamp: timeutil::now_utc_timestamp()
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned()),
            operation: HistoryOperation::Upload,
            page_url,
            files,
            bytes,
            result,
            delete_key,
        }
    }

    pub fn delete(page_url: String, files: Vec<String>, result: String) -> Self {
        Self {
            timestamp: timeutil::now_utc_timestamp()
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned()),
            operation: HistoryOperation::Delete,
            page_url,
            files,
            bytes: None,
            result,
            delete_key: None,
        }
    }
}

pub fn settings(
    config: &AppConfig,
    override_mode: HistoryOverride,
) -> Result<HistorySettings, GfileError> {
    let enabled = match override_mode {
        HistoryOverride::Auto => config.history.enabled.unwrap_or(false),
        HistoryOverride::Enable => true,
        HistoryOverride::Disable => false,
    };
    Ok(HistorySettings {
        enabled,
        store_delete_keys: config.history.store_delete_keys.unwrap_or(false),
        path: default_history_path()?,
    })
}

pub fn default_history_path() -> Result<PathBuf, GfileError> {
    if let Some(path) = env::var_os("RGFILE_TEST_DATA_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path).join("rgfile").join("history.jsonl"));
    }
    let base_dirs = BaseDirs::new().ok_or_else(|| GfileError::Usage {
        message: "could not determine platform data directory for history".to_owned(),
    })?;
    Ok(base_dirs.data_dir().join("rgfile").join("history.jsonl"))
}

pub fn append(settings: &HistorySettings, record: &HistoryRecord) {
    if !settings.enabled {
        return;
    }
    if let Err(error) = append_record(&settings.path, record) {
        eprintln!("Warning: failed to write history: {error}");
    }
}

pub fn append_record(path: &Path, record: &HistoryRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut file, record).map_err(io::Error::other)?;
    use std::io::Write as _;
    writeln!(file)?;
    Ok(())
}

pub fn read(path: &Path) -> Result<Vec<HistoryRecord>, GfileError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(source, path, IoOp::Read)),
    };

    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<HistoryRecord>(line).map_err(|source| GfileError::Parse {
                what: format!("history line {} is not valid JSON: {source}", index + 1),
                hint: "Run `rgfile history clear` if the history file is corrupt.".to_owned(),
            })
        })
        .collect()
}

pub fn latest(mut records: Vec<HistoryRecord>, limit: usize) -> Vec<HistoryRecord> {
    records.reverse();
    records.truncate(limit);
    records
}

pub fn clear(path: &Path) -> Result<(), GfileError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(source, path, IoOp::Write)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_and_clear_history() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let first = HistoryRecord::download(
            "https://23.gigafile.nu/0123abcd-000000example".to_owned(),
            vec!["one.bin".to_owned()],
            Some(1),
            "ok".to_owned(),
        );
        let second = HistoryRecord::upload(
            "https://23.gigafile.nu/0123abcd-000000example".to_owned(),
            vec!["two.bin".to_owned()],
            Some(2),
            "19".to_owned(),
            None,
        );

        append_record(&path, &first).unwrap();
        append_record(&path, &second).unwrap();

        let records = read(&path).unwrap();
        assert_eq!(records, vec![first, second.clone()]);
        assert_eq!(latest(records, 1), vec![second]);

        clear(&path).unwrap();
        assert!(read(&path).unwrap().is_empty());
    }

    #[test]
    fn missing_history_is_empty_and_clear_is_ok() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("missing.jsonl");

        assert!(read(&path).unwrap().is_empty());
        clear(&path).unwrap();
    }
}
