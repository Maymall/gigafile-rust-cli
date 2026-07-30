// SPDX-License-Identifier: MIT

use std::{
    fs, io,
    io::{BufRead, BufReader, Write as _},
    path::{Path, PathBuf},
};

#[cfg(debug_assertions)]
use std::env;

use directories::BaseDirs;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

use crate::{
    config::AppConfig,
    error::{GfileError, IoOp, io_error},
    timeutil,
};

const MAX_HISTORY_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
enum HistoryOpenMode {
    Read,
    Append,
}

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
    #[cfg(debug_assertions)]
    {
        if let Some(path) = env::var_os("RGFILE_TEST_DATA_DIR").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(path).join("rgfile").join("history.jsonl"));
        }
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
    let mut encoded = serde_json::to_vec(record).map_err(io::Error::other)?;
    if encoded.len() > MAX_HISTORY_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("history record exceeds the {MAX_HISTORY_LINE_BYTES}-byte line limit"),
        ));
    }
    encoded.push(b'\n');

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    checked_path_metadata(path)?;

    let coordination_file = open_coordination_file(path)?;
    FileExt::lock_exclusive(&coordination_file)?;
    validate_open_file_path(&coordination_path(path), &coordination_file)?;

    let mut file = open_history_file(path, HistoryOpenMode::Append)?
        .expect("append mode always creates the history file");
    FileExt::lock_exclusive(&file)?;
    validate_open_file_path(path, &file)?;
    file.write_all(&encoded)
}

pub fn read(path: &Path) -> Result<Vec<HistoryRecord>, GfileError> {
    let Some((_coordination_file, file)) = locked_history_file(path)? else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    read_history_records(path, file, |record| records.push(record))?;
    Ok(records)
}

pub fn read_latest(path: &Path, limit: usize) -> Result<Vec<HistoryRecord>, GfileError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let Some((_coordination_file, file)) = locked_history_file(path)? else {
        return Ok(Vec::new());
    };
    // Do not trust a CLI/library caller's limit as an allocation size. The
    // deque will grow only as actual records are encountered.
    let mut records = std::collections::VecDeque::with_capacity(limit.min(1024));
    read_history_records(path, file, |record| {
        if records.len() == limit {
            records.pop_front();
        }
        records.push_back(record);
    })?;
    Ok(records.into_iter().rev().collect())
}

pub fn read_latest_matching(
    path: &Path,
    mut predicate: impl FnMut(&HistoryRecord) -> bool,
) -> Result<Option<HistoryRecord>, GfileError> {
    let Some((_coordination_file, file)) = locked_history_file(path)? else {
        return Ok(None);
    };
    let mut latest = None;
    read_history_records(path, file, |record| {
        if predicate(&record) {
            latest = Some(record);
        }
    })?;
    Ok(latest)
}

fn read_history_records(
    path: &Path,
    file: fs::File,
    mut visit: impl FnMut(HistoryRecord),
) -> Result<(), GfileError> {
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut index = 0;
    while read_history_line(&mut reader, &mut line, path, index)? {
        if !std::str::from_utf8(&line).is_ok_and(|line| line.trim().is_empty()) {
            visit(parse_history_line(index, &line)?);
        }
        index += 1;
    }
    Ok(())
}

fn read_history_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    path: &Path,
    index: usize,
) -> Result<bool, GfileError> {
    line.clear();
    loop {
        let (payload_len, consumed, has_newline) = {
            let available = reader
                .fill_buf()
                .map_err(|source| io_error(source, path, IoOp::Read))?;
            if available.is_empty() {
                return Ok(!line.is_empty());
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let payload_len = newline.unwrap_or(available.len());
            if payload_len > MAX_HISTORY_LINE_BYTES - line.len() {
                return Err(history_line_too_large(index));
            }
            line.extend_from_slice(&available[..payload_len]);
            (
                payload_len,
                payload_len + usize::from(newline.is_some()),
                newline.is_some(),
            )
        };
        debug_assert!(consumed >= payload_len);
        reader.consume(consumed);
        if has_newline {
            return Ok(true);
        }
    }
}

fn history_line_too_large(index: usize) -> GfileError {
    GfileError::Parse {
        what: format!(
            "history line {} exceeds the {MAX_HISTORY_LINE_BYTES}-byte safety limit",
            index + 1
        ),
        hint: "Run `rgfile history clear` if the history file is corrupt.".to_owned(),
    }
}

fn parse_history_line(index: usize, line: &[u8]) -> Result<HistoryRecord, GfileError> {
    serde_json::from_slice::<HistoryRecord>(line).map_err(|source| GfileError::Parse {
        what: format!("history line {} is not valid JSON: {source}", index + 1),
        hint: "Run `rgfile history clear` if the history file is corrupt.".to_owned(),
    })
}

fn locked_history_file(path: &Path) -> Result<Option<(fs::File, fs::File)>, GfileError> {
    match checked_path_metadata(path) {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(None),
        Err(source) => return Err(io_error(source, path, IoOp::Read)),
    }
    let coordination_file =
        open_coordination_file(path).map_err(|source| io_error(source, path, IoOp::Read))?;
    FileExt::lock_shared(&coordination_file)
        .map_err(|source| io_error(source, path, IoOp::Read))?;
    validate_open_file_path(&coordination_path(path), &coordination_file)
        .map_err(|source| io_error(source, path, IoOp::Read))?;
    let Some(file) = open_history_file(path, HistoryOpenMode::Read)
        .map_err(|source| io_error(source, path, IoOp::Read))?
    else {
        return Ok(None);
    };
    FileExt::lock_shared(&file).map_err(|source| io_error(source, path, IoOp::Read))?;
    validate_open_file_path(path, &file).map_err(|source| io_error(source, path, IoOp::Read))?;
    Ok(Some((coordination_file, file)))
}

pub fn latest(mut records: Vec<HistoryRecord>, limit: usize) -> Vec<HistoryRecord> {
    records.reverse();
    records.truncate(limit);
    records
}

pub fn clear(path: &Path) -> Result<(), GfileError> {
    match checked_path_metadata(path) {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(()),
        Err(source) => return Err(io_error(source, path, IoOp::Write)),
    }

    // The stable sidecar lock lets clear remove the history path while holding
    // the lock. Locking and deleting the history file itself is racy on Unix
    // and is rejected by Windows while the file handle remains open.
    let coordination_file =
        open_coordination_file(path).map_err(|source| io_error(source, path, IoOp::Write))?;
    FileExt::lock_exclusive(&coordination_file)
        .map_err(|source| io_error(source, path, IoOp::Write))?;
    validate_open_file_path(&coordination_path(path), &coordination_file)
        .map_err(|source| io_error(source, path, IoOp::Write))?;

    #[cfg(unix)]
    let history_file = {
        let Some(file) = open_history_file(path, HistoryOpenMode::Read)
            .map_err(|source| io_error(source, path, IoOp::Write))?
        else {
            return Ok(());
        };
        FileExt::lock_exclusive(&file).map_err(|source| io_error(source, path, IoOp::Write))?;
        validate_open_file_path(path, &file)
            .map_err(|source| io_error(source, path, IoOp::Write))?;
        file
    };

    #[cfg(not(unix))]
    match checked_path_metadata(path) {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(()),
        Err(source) => return Err(io_error(source, path, IoOp::Write)),
    }

    let result = match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(source, path, IoOp::Write)),
    };
    #[cfg(unix)]
    drop(history_file);
    result
}

fn coordination_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn open_coordination_file(path: &Path) -> io::Result<fs::File> {
    let lock_path = coordination_path(path);
    let before = checked_path_metadata(&lock_path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    configure_no_follow(&mut options);

    let file = options.open(&lock_path)?;
    validate_open_file(&lock_path, before.as_ref(), &file)?;
    restrict_permissions(&file)?;
    validate_open_file_path(&lock_path, &file)?;
    Ok(file)
}

fn open_history_file(path: &Path, mode: HistoryOpenMode) -> io::Result<Option<fs::File>> {
    let before = checked_path_metadata(path)?;
    if before.is_none() && matches!(mode, HistoryOpenMode::Read) {
        return Ok(None);
    }

    let mut options = fs::OpenOptions::new();
    options.read(true);
    match mode {
        HistoryOpenMode::Read => {}
        HistoryOpenMode::Append => {
            options.create(true).append(true);
            #[cfg(unix)]
            options.mode(0o600);
        }
    }
    configure_no_follow(&mut options);

    let file = match options.open(path) {
        Ok(file) => file,
        Err(error)
            if matches!(mode, HistoryOpenMode::Read) && error.kind() == io::ErrorKind::NotFound =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    validate_open_file(path, before.as_ref(), &file)?;
    restrict_permissions(&file)?;
    validate_open_file_path(path, &file)?;
    Ok(Some(file))
}

fn checked_path_metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_regular_file(&metadata)?;
    Ok(Some(metadata))
}

fn validate_open_file(
    path: &Path,
    before: Option<&fs::Metadata>,
    file: &fs::File,
) -> io::Result<()> {
    let opened = file.metadata()?;
    validate_regular_file(&opened)?;
    let after = fs::symlink_metadata(path)?;
    validate_regular_file(&after)?;
    if !same_file_identity(&opened, &after)
        || before.is_some_and(|before| !same_file_identity(before, &opened))
    {
        return Err(unsafe_history_file_error(
            "file identity changed while it was being opened",
        ));
    }
    Ok(())
}

fn validate_open_file_path(path: &Path, file: &fs::File) -> io::Result<()> {
    let opened = file.metadata()?;
    validate_regular_file(&opened)?;
    let current = fs::symlink_metadata(path)?;
    validate_regular_file(&current)?;
    if !same_file_identity(&opened, &current) {
        return Err(unsafe_history_file_error(
            "file identity changed while it was locked",
        ));
    }
    Ok(())
}

fn validate_regular_file(metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.file_type().is_file() {
        return Err(unsafe_history_file_error(
            "expected a regular file, not a symlink or special file",
        ));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(unsafe_history_file_error(
            "refusing a file with multiple hard links",
        ));
    }
    Ok(())
}

fn unsafe_history_file_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(unix)]
fn configure_no_follow(options: &mut fs::OpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn configure_no_follow(_options: &mut fs::OpenOptions) {}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn restrict_permissions(file: &fs::File) -> io::Result<()> {
    let permissions = file.metadata()?.permissions();
    if permissions.mode() & 0o7777 != 0o600 {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPEND_WORKER_PATH: &str = "RGFILE_HISTORY_APPEND_WORKER_PATH";
    const APPEND_WORKER_ID: &str = "RGFILE_HISTORY_APPEND_WORKER_ID";
    const APPEND_WORKER_COUNT: &str = "RGFILE_HISTORY_APPEND_WORKER_COUNT";
    const APPEND_WORKER_TEST: &str = "history::tests::append_record_process_worker";

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
        assert!(!coordination_path(&path).exists());
    }

    #[test]
    fn oversized_history_lines_are_rejected_by_all_readers() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let mut oversized = vec![b'x'; MAX_HISTORY_LINE_BYTES + 1];
        oversized.push(b'\n');
        fs::write(&path, oversized).unwrap();

        for error in [
            read(&path).unwrap_err(),
            read_latest(&path, 1).unwrap_err(),
            read_latest_matching(&path, |_| true).unwrap_err(),
        ] {
            assert_eq!(error.exit_code(), 13);
            assert!(
                error
                    .user_message()
                    .contains("history line 1 exceeds the 1048576-byte safety limit"),
                "{}",
                error.user_message()
            );
        }
    }

    #[test]
    fn append_rejects_records_over_the_history_line_limit() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let record = HistoryRecord::download(
            "https://example.test/history".to_owned(),
            vec!["x".repeat(MAX_HISTORY_LINE_BYTES)],
            None,
            "ok".to_owned(),
        );

        let error = append_record(&path, &record).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("1048576-byte line limit"));
        assert!(!path.exists());
    }

    #[test]
    fn latest_matching_streams_a_large_history_and_keeps_the_last_match() {
        const RECORDS: usize = 25_000;

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let file = fs::File::create(&path).unwrap();
        let mut writer = io::BufWriter::new(file);
        for index in 0..RECORDS {
            let record = HistoryRecord {
                timestamp: "2026-07-19T00:00:00Z".to_owned(),
                operation: HistoryOperation::Upload,
                page_url: if index % 4_096 == 0 || index == RECORDS - 7 {
                    "https://example.test/match".to_owned()
                } else {
                    format!("https://example.test/{index}")
                },
                files: vec![format!("file-{index}.bin")],
                bytes: Some(index as u64),
                result: "ok".to_owned(),
                delete_key: Some(format!("key-{index}")),
            };
            serde_json::to_writer(&mut writer, &record).unwrap();
            writer.write_all(b"\n").unwrap();
        }
        writer.flush().unwrap();

        let matched = read_latest_matching(&path, |record| {
            record.operation == HistoryOperation::Upload
                && record.page_url == "https://example.test/match"
                && record.delete_key.is_some()
        })
        .unwrap()
        .unwrap();

        assert_eq!(matched.bytes, Some((RECORDS - 7) as u64));
        assert_eq!(matched.delete_key, Some(format!("key-{}", RECORDS - 7)));
    }

    #[test]
    fn latest_matching_still_reports_corruption_after_a_match() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let matching = HistoryRecord::upload(
            "https://example.test/match".to_owned(),
            vec!["file.bin".to_owned()],
            Some(7),
            "ok".to_owned(),
            Some("key".to_owned()),
        );
        let mut encoded = serde_json::to_vec(&matching).unwrap();
        encoded.extend_from_slice(b"\ntruncated {\n");
        fs::write(&path, encoded).unwrap();

        let error =
            read_latest_matching(&path, |record| record.page_url.ends_with("/match")).unwrap_err();

        assert_eq!(error.exit_code(), 13);
        assert!(error.user_message().contains("history line 2"));
    }

    #[test]
    fn concurrent_processes_append_complete_records() {
        use std::{collections::HashSet, process::Command};

        const WORKERS: usize = 6;
        const RECORDS_PER_WORKER: usize = 40;

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let executable = env::current_exe().unwrap();
        let mut children = Vec::with_capacity(WORKERS);

        for worker in 0..WORKERS {
            let child = Command::new(&executable)
                .args(["--exact", APPEND_WORKER_TEST, "--ignored"])
                .env(APPEND_WORKER_PATH, &path)
                .env(APPEND_WORKER_ID, worker.to_string())
                .env(APPEND_WORKER_COUNT, RECORDS_PER_WORKER.to_string())
                .spawn()
                .unwrap();
            children.push(child);
        }

        for mut child in children {
            let status = child.wait().unwrap();
            assert!(status.success(), "history append worker failed: {status}");
        }

        let records = read(&path).unwrap();
        assert_eq!(records.len(), WORKERS * RECORDS_PER_WORKER);
        let pages = records
            .iter()
            .map(|record| record.page_url.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(pages.len(), WORKERS * RECORDS_PER_WORKER);
        for worker in 0..WORKERS {
            for index in 0..RECORDS_PER_WORKER {
                assert!(pages.contains(format!("worker-{worker}-{index}").as_str()));
            }
        }
    }

    #[test]
    #[ignore = "subprocess worker for concurrent_processes_append_complete_records"]
    fn append_record_process_worker() {
        let Some(path) = env::var_os(APPEND_WORKER_PATH).map(PathBuf::from) else {
            return;
        };
        let worker = env::var(APPEND_WORKER_ID)
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let count = env::var(APPEND_WORKER_COUNT)
            .unwrap()
            .parse::<usize>()
            .unwrap();

        for index in 0..count {
            let record = HistoryRecord {
                timestamp: "2026-07-19T00:00:00Z".to_owned(),
                operation: HistoryOperation::Upload,
                page_url: format!("worker-{worker}-{index}"),
                files: vec![format!("file-{worker}-{index}.bin")],
                bytes: Some(index as u64),
                result: "ok".to_owned(),
                delete_key: None,
            };
            append_record(&path, &record).unwrap();
        }
    }

    #[test]
    fn read_waits_for_an_exclusive_history_operation() {
        use std::{sync::mpsc, thread, time::Duration};

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let record = HistoryRecord::download(
            "https://example.test/history".to_owned(),
            vec!["file.bin".to_owned()],
            Some(7),
            "ok".to_owned(),
        );
        append_record(&path, &record).unwrap();

        let coordination_file = open_coordination_file(&path).unwrap();
        FileExt::lock_exclusive(&coordination_file).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let read_path = path.clone();
        let reader = thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx.send(read(&read_path)).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(result_rx.recv_timeout(Duration::from_millis(150)).is_err());
        FileExt::unlock(&coordination_file).unwrap();
        assert_eq!(
            result_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap(),
            vec![record]
        );
        reader.join().unwrap();
    }

    #[test]
    fn clear_waits_for_append_coordination_and_removes_history() {
        use std::{sync::mpsc, thread, time::Duration};

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let record = HistoryRecord::download(
            "https://example.test/history".to_owned(),
            vec!["file.bin".to_owned()],
            Some(7),
            "ok".to_owned(),
        );
        append_record(&path, &record).unwrap();

        let coordination_file = open_coordination_file(&path).unwrap();
        FileExt::lock_exclusive(&coordination_file).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let clear_path = path.clone();
        let clearer = thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx.send(clear(&clear_path)).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(result_rx.recv_timeout(Duration::from_millis(150)).is_err());
        FileExt::unlock(&coordination_file).unwrap();
        result_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        clearer.join().unwrap();
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn read_and_append_restrict_existing_history_permissions() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let new_path = temp.path().join("new-history.jsonl");
        let first = HistoryRecord::download(
            "https://example.test/first".to_owned(),
            vec!["first.bin".to_owned()],
            Some(1),
            "ok".to_owned(),
        );
        let second = HistoryRecord::download(
            "https://example.test/second".to_owned(),
            vec!["second.bin".to_owned()],
            Some(2),
            "ok".to_owned(),
        );

        append_record(&new_path, &first).unwrap();
        assert_eq!(
            fs::metadata(&new_path).unwrap().permissions().mode() & 0o7777,
            0o600
        );

        let mut encoded = serde_json::to_vec(&first).unwrap();
        encoded.push(b'\n');
        fs::write(&path, encoded).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        assert_eq!(read(&path).unwrap(), vec![first]);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        append_record(&path, &second).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn history_symlink_is_rejected_without_changing_its_victim() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let victim = temp.path().join("victim.jsonl");
        let path = temp.path().join("history.jsonl");
        let original = b"do not touch\n";
        fs::write(&victim, original).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&victim, &path).unwrap();
        let record = HistoryRecord::download(
            "https://example.test/history".to_owned(),
            vec!["file.bin".to_owned()],
            Some(7),
            "ok".to_owned(),
        );

        assert!(append_record(&path, &record).is_err());
        assert!(read(&path).is_err());
        assert!(read_latest(&path, 1).is_err());
        assert!(read_latest_matching(&path, |_| true).is_err());
        assert!(clear(&path).is_err());

        assert_eq!(fs::read(&victim).unwrap(), original);
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn coordination_symlink_is_rejected_without_changing_its_victim() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("history.jsonl");
        let victim = temp.path().join("victim.lock");
        let lock_path = coordination_path(&path);
        let record = HistoryRecord::download(
            "https://example.test/history".to_owned(),
            vec!["file.bin".to_owned()],
            Some(7),
            "ok".to_owned(),
        );
        let mut history = serde_json::to_vec(&record).unwrap();
        history.push(b'\n');
        fs::write(&path, &history).unwrap();
        let original = b"do not touch\n";
        fs::write(&victim, original).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&victim, &lock_path).unwrap();

        assert!(append_record(&path, &record).is_err());
        assert!(read(&path).is_err());
        assert!(read_latest(&path, 1).is_err());
        assert!(read_latest_matching(&path, |_| true).is_err());
        assert!(clear(&path).is_err());

        assert_eq!(fs::read(&victim).unwrap(), original);
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert_eq!(fs::read(&path).unwrap(), history);
        assert!(
            fs::symlink_metadata(&lock_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn history_and_coordination_hard_links_are_rejected() {
        let temp = tempfile::TempDir::new().unwrap();
        let history_victim = temp.path().join("history-victim");
        let history_path = temp.path().join("history.jsonl");
        fs::write(&history_victim, b"history victim\n").unwrap();
        fs::hard_link(&history_victim, &history_path).unwrap();
        assert!(read(&history_path).is_err());
        assert_eq!(fs::read(&history_victim).unwrap(), b"history victim\n");

        let safe_history = temp.path().join("safe-history.jsonl");
        let lock_victim = temp.path().join("lock-victim");
        let lock_path = coordination_path(&safe_history);
        let record = HistoryRecord::download(
            "https://example.test/history".to_owned(),
            vec!["file.bin".to_owned()],
            Some(7),
            "ok".to_owned(),
        );
        let mut encoded = serde_json::to_vec(&record).unwrap();
        encoded.push(b'\n');
        fs::write(&safe_history, encoded).unwrap();
        fs::write(&lock_victim, b"lock victim\n").unwrap();
        fs::hard_link(&lock_victim, &lock_path).unwrap();

        assert!(read(&safe_history).is_err());
        assert_eq!(fs::read(&lock_victim).unwrap(), b"lock victim\n");
    }
}
