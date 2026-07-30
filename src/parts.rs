// SPDX-License-Identifier: MIT

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read as _},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::{
    download,
    error::{GfileError, IoOp, io_error},
};

#[derive(Debug, Clone, Serialize)]
pub struct PartsReport {
    pub status: &'static str,
    #[serde(serialize_with = "serialize_path_lossy")]
    pub dir: PathBuf,
    pub groups: Vec<PartGroup>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PartGroup {
    pub target_name: String,
    #[serde(serialize_with = "serialize_path_lossy")]
    pub target_path: PathBuf,
    #[serde(serialize_with = "serialize_optional_path_lossy")]
    pub part_path: Option<PathBuf>,
    #[serde(serialize_with = "serialize_optional_path_lossy")]
    pub sidecar_path: Option<PathBuf>,
    #[serde(serialize_with = "serialize_optional_path_lossy")]
    pub sidecar_tmp_path: Option<PathBuf>,
    #[serde(serialize_with = "serialize_optional_path_lossy")]
    pub lock_path: Option<PathBuf>,
    pub state: PartState,
    pub active: bool,
    pub disk_bytes: u64,
    pub completed_bytes: Option<u64>,
    pub expected_bytes: Option<u64>,
    pub progress_percent: Option<f64>,
    pub mtime_unix: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartState {
    Resumable,
    PartWithoutSidecar,
    SidecarWithoutPart,
    LockOnly,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanReport {
    pub status: &'static str,
    #[serde(serialize_with = "serialize_path_lossy")]
    pub dir: PathBuf,
    pub deleted: Vec<CleanedGroup>,
    pub skipped_active: Vec<PartGroup>,
    pub failed: Vec<CleanFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanedGroup {
    pub target_name: String,
    #[serde(serialize_with = "serialize_paths_lossy")]
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanFailure {
    pub target_name: String,
    #[serde(serialize_with = "serialize_path_lossy")]
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
struct GroupBuilder {
    part: Option<FileSample>,
    sidecar: Option<FileSample>,
    sidecar_tmp: Option<FileSample>,
    lock: Option<FileSample>,
}

#[derive(Debug)]
struct FileSample {
    path: PathBuf,
    metadata: fs::Metadata,
}

struct CleanLock {
    _file: File,
    path: PathBuf,
}

enum CleanLockState {
    Acquired(CleanLock),
    Active,
}

#[derive(Debug, Clone, Copy)]
enum PartFileKind {
    Part,
    Sidecar,
    SidecarTmp,
    Lock,
}

pub fn list(dir: PathBuf) -> Result<PartsReport, GfileError> {
    let mut builders = BTreeMap::<OsString, GroupBuilder>::new();
    for entry in fs::read_dir(&dir).map_err(|source| io_error(source, &dir, IoOp::Read))? {
        let entry = entry.map_err(|source| io_error(source, &dir, IoOp::Read))?;
        let path = entry.path();
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name() else {
            continue;
        };
        let Some((target_name, kind)) = classify_part_file(file_name) else {
            continue;
        };
        let builder = builders.entry(target_name).or_default();
        let sample = FileSample { path, metadata };
        match kind {
            PartFileKind::Part => builder.part = Some(sample),
            PartFileKind::Sidecar => builder.sidecar = Some(sample),
            PartFileKind::SidecarTmp => builder.sidecar_tmp = Some(sample),
            PartFileKind::Lock => builder.lock = Some(sample),
        }
    }

    let groups = builders
        .into_iter()
        .map(|(target_name, builder)| build_group(&dir, target_name, builder))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(PartsReport {
        status: "ok",
        dir,
        groups,
    })
}

pub fn clean(
    dir: PathBuf,
    groups: &[PartGroup],
    older_than: Option<Duration>,
) -> Result<CleanReport, GfileError> {
    let mut deleted = Vec::new();
    let mut skipped_active = Vec::new();
    let mut failed = Vec::new();

    for group in groups {
        if !matches_older_than(group, older_than) {
            continue;
        }
        if group.active {
            skipped_active.push(group.clone());
            continue;
        }

        // Hold the same lock used by the downloader for the complete delete.
        // A one-shot probe leaves a gap in which a new download can start after
        // the probe but before its .part is removed.
        let lock_path = clean_lock_path(group);
        let clean_lock = match acquire_clean_lock(&lock_path) {
            Ok(CleanLockState::Acquired(lock)) => lock,
            Ok(CleanLockState::Active) => {
                skipped_active.push(group.clone());
                continue;
            }
            Err(error) => {
                failed.push(CleanFailure {
                    target_name: group.target_name.clone(),
                    path: lock_path,
                    message: error.to_string(),
                });
                continue;
            }
        };

        let mut removed_paths = Vec::new();
        let mut paths = group_paths(group);
        if !paths.contains(&clean_lock.path) {
            paths.push(clean_lock.path.clone());
        }
        paths.sort_by_key(|path| path == &clean_lock.path);
        for path in paths {
            match fs::remove_file(&path) {
                Ok(()) => removed_paths.push(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => failed.push(CleanFailure {
                    target_name: group.target_name.clone(),
                    path,
                    message: error.to_string(),
                }),
            }
        }
        if !removed_paths.is_empty() {
            deleted.push(CleanedGroup {
                target_name: group.target_name.clone(),
                paths: removed_paths,
            });
        }
    }

    let status = if failed.is_empty() { "ok" } else { "partial" };
    Ok(CleanReport {
        status,
        dir,
        deleted,
        skipped_active,
        failed,
    })
}

pub fn clean_candidates(groups: &[PartGroup], older_than: Option<Duration>) -> Vec<PartGroup> {
    groups
        .iter()
        .filter(|group| !group.active && matches_older_than(group, older_than))
        .cloned()
        .collect()
}

fn build_group(
    dir: &Path,
    target_name_os: OsString,
    builder: GroupBuilder,
) -> Result<PartGroup, GfileError> {
    let target_path = dir.join(&target_name_os);
    let target_name = target_name_os.to_string_lossy().into_owned();
    let state = match (
        &builder.part,
        &builder.sidecar,
        &builder.sidecar_tmp,
        &builder.lock,
    ) {
        (Some(_), Some(_), _, _) => PartState::Resumable,
        (Some(_), None, _, _) => PartState::PartWithoutSidecar,
        (None, Some(_), _, _) | (None, None, Some(_), _) => PartState::SidecarWithoutPart,
        (None, None, None, Some(_)) => PartState::LockOnly,
        (None, None, None, None) => PartState::LockOnly,
    };
    let active = builder
        .lock
        .as_ref()
        .is_some_and(|sample| lock_is_active(&sample.path).unwrap_or(false));
    let part_bytes = builder.part.as_ref().map(|sample| sample.metadata.len());
    let (expected_bytes, completed_bytes) = match builder.sidecar.as_ref() {
        Some(sidecar) => sidecar_progress(sidecar, part_bytes),
        None => (None, part_bytes),
    };
    let progress_percent = completed_bytes
        .zip(expected_bytes)
        .and_then(|(completed, expected)| {
            (expected > 0).then_some(completed as f64 / expected as f64 * 100.0)
        });
    let paths = [
        builder.part.as_ref(),
        builder.sidecar.as_ref(),
        builder.sidecar_tmp.as_ref(),
        builder.lock.as_ref(),
    ];
    let disk_bytes = paths
        .iter()
        .flatten()
        .map(|sample| sample.metadata.len())
        .sum();
    let mtime_unix = paths
        .iter()
        .filter_map(|sample| {
            sample
                .and_then(|sample| sample.metadata.modified().ok())
                .and_then(system_time_unix)
        })
        .max();

    Ok(PartGroup {
        target_name,
        target_path,
        part_path: builder.part.map(|sample| sample.path),
        sidecar_path: builder.sidecar.map(|sample| sample.path),
        sidecar_tmp_path: builder.sidecar_tmp.map(|sample| sample.path),
        lock_path: builder.lock.map(|sample| sample.path),
        state,
        active,
        disk_bytes,
        completed_bytes,
        expected_bytes,
        progress_percent,
        mtime_unix,
    })
}

fn classify_part_file(file_name: &OsStr) -> Option<(OsString, PartFileKind)> {
    if let Some(target) = strip_extensions(file_name, &["lock", "json", "part"]) {
        return Some((target, PartFileKind::Lock));
    }
    if let Some(target) = strip_extensions(file_name, &["tmp", "json", "part"]) {
        return Some((target, PartFileKind::SidecarTmp));
    }
    if let Some(target) = strip_extensions(file_name, &["json", "part"]) {
        return Some((target, PartFileKind::Sidecar));
    }
    if let Some(target) = strip_extensions(file_name, &["part"]) {
        return Some((target, PartFileKind::Part));
    }
    None
}

fn strip_extensions(file_name: &OsStr, extensions: &[&str]) -> Option<OsString> {
    let mut remaining = OsString::from(file_name);
    for extension in extensions {
        let path = Path::new(&remaining);
        if path.extension()? != OsStr::new(extension) {
            return None;
        }
        remaining = path.file_stem()?.to_os_string();
    }
    (!remaining.is_empty()).then_some(remaining)
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_default();
    file_name.push(suffix);
    path.with_file_name(file_name)
}

fn clean_lock_path(group: &PartGroup) -> PathBuf {
    group
        .lock_path
        .clone()
        .unwrap_or_else(|| path_with_suffix(&group.target_path, ".part.json.lock"))
}

#[allow(clippy::ptr_arg)]
fn serialize_path_lossy<S>(path: &PathBuf, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&path.to_string_lossy())
}

fn serialize_optional_path_lossy<S>(
    path: &Option<PathBuf>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    path.as_ref()
        .map(|path| path.to_string_lossy())
        .serialize(serializer)
}

fn serialize_paths_lossy<S>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .serialize(serializer)
}

fn acquire_clean_lock(path: &Path) -> Result<CleanLockState, io::Error> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(file) => file,
        Err(source) if download::is_lock_contention(&source) => {
            return Ok(CleanLockState::Active);
        }
        Err(source) => return Err(source),
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(CleanLockState::Acquired(CleanLock {
            _file: file,
            path: path.to_owned(),
        })),
        Err(source) if download::is_lock_contention(&source) => Ok(CleanLockState::Active),
        Err(source) => Err(source),
    }
}

fn lock_is_active(path: &Path) -> Result<bool, GfileError> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(source) if download::is_lock_contention(&source) => return Ok(true),
        Err(source) => return Err(io_error(source, path, IoOp::Read)),
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            Ok(false)
        }
        Err(source) if download::is_lock_contention(&source) => Ok(true),
        Err(source) => Err(io_error(source, path, IoOp::Read)),
    }
}

fn sidecar_progress(sidecar: &FileSample, part_bytes: Option<u64>) -> (Option<u64>, Option<u64>) {
    let value = match read_sidecar_value(sidecar) {
        Ok(Some(value)) => value,
        Ok(None) => return (None, None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (None, part_bytes),
        Err(_) => return (None, None),
    };
    let Some(version) = value.get("version").and_then(Value::as_u64) else {
        return (None, None);
    };
    let expected = matches!(version, 1 | 2)
        .then(|| value.get("expected").and_then(Value::as_u64))
        .flatten();
    let completed = match version {
        1 => part_bytes,
        2 => v2_completed_bytes(&value, part_bytes),
        _ => None,
    };
    (expected, completed)
}

fn read_sidecar_value(sidecar: &FileSample) -> io::Result<Option<Value>> {
    const MAX_SIDECAR_BYTES: u64 = 1024 * 1024;

    if !sidecar.metadata.is_file() || sidecar.metadata.len() > MAX_SIDECAR_BYTES {
        return Ok(None);
    }
    let file = File::open(&sidecar.path)?;
    let mut bytes = Vec::with_capacity(sidecar.metadata.len() as usize);
    file.take(MAX_SIDECAR_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

fn v2_completed_bytes(value: &Value, part_bytes: Option<u64>) -> Option<u64> {
    value.get("file_id")?.as_str()?;
    value.get("key_used")?.as_bool()?;
    if !valid_sidecar_validator(value.get("validator")) {
        return None;
    }

    let expected = value.get("expected")?.as_u64()?;
    if expected == 0 || part_bytes != Some(expected) {
        return None;
    }
    let segments = value.get("segments")?.as_array()?;
    if segments.is_empty() || segments.len() > usize::from(download::MAX_DOWNLOAD_THREADS) {
        return None;
    }

    let mut expected_start = 0_u64;
    let mut completed = 0_u64;
    for segment in segments {
        let start = segment.get("start")?.as_u64()?;
        let end = segment.get("end")?.as_u64()?;
        let done = segment.get("done")?.as_bool()?;
        let downloaded = match segment.get("downloaded") {
            Some(value) => value.as_u64()?,
            None => 0,
        };
        if start != expected_start || end < start || end >= expected {
            return None;
        }
        let len = end.checked_sub(start)?.checked_add(1)?;
        if downloaded > len {
            return None;
        }
        completed = completed.checked_add(if done { len } else { downloaded })?;
        expected_start = end.checked_add(1)?;
    }
    (expected_start == expected).then_some(completed)
}

fn valid_sidecar_validator(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return true;
    };
    if value.is_null() {
        return true;
    }
    let Some(kind) = value.get("kind").and_then(Value::as_str) else {
        return false;
    };
    matches!(kind, "strong_etag" | "last_modified")
        && value.get("value").and_then(Value::as_str).is_some()
}

fn group_paths(group: &PartGroup) -> Vec<PathBuf> {
    [
        group.part_path.clone(),
        group.sidecar_path.clone(),
        group.sidecar_tmp_path.clone(),
        group.lock_path.clone(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn matches_older_than(group: &PartGroup, older_than: Option<Duration>) -> bool {
    let Some(age) = older_than else {
        return true;
    };
    let Some(mtime) = group.mtime_unix else {
        return false;
    };
    let Some(cutoff) = SystemTime::now()
        .checked_sub(age)
        .and_then(system_time_unix)
    else {
        return false;
    };
    mtime <= cutoff
}

fn system_time_unix(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    use std::os::unix::ffi::OsStringExt as _;

    #[test]
    fn list_groups_part_sidecar_and_lock_states() {
        let temp = tempfile::TempDir::new().unwrap();
        write_v1(temp.path().join("seq.bin.part.json"), 100);
        fs::write(temp.path().join("seq.bin.part"), vec![0_u8; 40]).unwrap();
        fs::write(temp.path().join("orphan.bin.part"), vec![0_u8; 5]).unwrap();
        write_v2(temp.path().join("seg.bin.part.json"), 200, 50);
        fs::write(temp.path().join("seg.bin.part"), vec![0_u8; 200]).unwrap();
        fs::write(temp.path().join("old.bin.part.json.lock"), b"").unwrap();
        fs::write(temp.path().join("tmp.bin.part"), vec![0_u8; 5]).unwrap();
        fs::write(temp.path().join("tmp.bin.part.json.tmp"), b"pending").unwrap();

        let report = list(temp.path().to_owned()).unwrap();

        assert_eq!(report.groups.len(), 5);
        let seq = group(&report, "seq.bin");
        assert_eq!(seq.state, PartState::Resumable);
        assert_eq!(seq.completed_bytes, Some(40));
        assert_eq!(seq.expected_bytes, Some(100));
        let seg = group(&report, "seg.bin");
        assert_eq!(seg.completed_bytes, Some(50));
        assert_eq!(seg.expected_bytes, Some(200));
        assert_eq!(
            group(&report, "orphan.bin").state,
            PartState::PartWithoutSidecar
        );
        assert_eq!(group(&report, "old.bin").state, PartState::LockOnly);
        let tmp = group(&report, "tmp.bin");
        let tmp_sidecar = temp.path().join("tmp.bin.part.json.tmp");
        assert_eq!(tmp.state, PartState::PartWithoutSidecar);
        assert_eq!(tmp.sidecar_tmp_path.as_deref(), Some(tmp_sidecar.as_path()));
    }

    #[test]
    fn list_keeps_expected_but_not_completed_for_invalid_v2_sidecar() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("invalid.bin.part"), vec![0_u8; 200]).unwrap();
        fs::write(
            temp.path().join("invalid.bin.part.json"),
            serde_json::json!({
                "version": 2,
                "file_id": "0123abcd-000000example",
                "expected": 200,
                "key_used": false,
                "segments": [
                    {"start": 0, "end": 99, "done": true, "downloaded": 100},
                    {"start": 101, "end": 199, "done": false, "downloaded": 0}
                ]
            })
            .to_string(),
        )
        .unwrap();

        let report = list(temp.path().to_owned()).unwrap();
        let group = group(&report, "invalid.bin");

        assert_eq!(group.expected_bytes, Some(200));
        assert_eq!(group.completed_bytes, None);
        assert_eq!(
            group.completed_bytes,
            download::bytes_completed_on_disk(
                &temp.path().join("invalid.bin.part"),
                &temp.path().join("invalid.bin.part.json")
            )
        );
        assert_eq!(group.progress_percent, None);
    }

    #[test]
    fn list_rejects_invalid_v2_validator_for_completed_progress() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("invalid.bin.part"), vec![0_u8; 100]).unwrap();
        fs::write(
            temp.path().join("invalid.bin.part.json"),
            serde_json::json!({
                "version": 2,
                "file_id": "0123abcd-000000example",
                "expected": 100,
                "key_used": false,
                "validator": {"kind": "unknown", "value": "\"fixture\""},
                "segments": [
                    {"start": 0, "end": 99, "done": false, "downloaded": 50}
                ]
            })
            .to_string(),
        )
        .unwrap();

        let report = list(temp.path().to_owned()).unwrap();
        let group = group(&report, "invalid.bin");

        assert_eq!(group.expected_bytes, Some(100));
        assert_eq!(group.completed_bytes, None);
        assert_eq!(
            group.completed_bytes,
            download::bytes_completed_on_disk(
                &temp.path().join("invalid.bin.part"),
                &temp.path().join("invalid.bin.part.json")
            )
        );
    }

    #[test]
    fn list_accepts_missing_v2_downloaded_as_zero() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("legacy.bin.part"), vec![0_u8; 100]).unwrap();
        fs::write(
            temp.path().join("legacy.bin.part.json"),
            serde_json::json!({
                "version": 2,
                "file_id": "0123abcd-000000example",
                "expected": 100,
                "key_used": false,
                "segments": [
                    {"start": 0, "end": 49, "done": true},
                    {"start": 50, "end": 99, "done": false}
                ]
            })
            .to_string(),
        )
        .unwrap();

        let report = list(temp.path().to_owned()).unwrap();

        assert_eq!(group(&report, "legacy.bin").completed_bytes, Some(50));
        assert_eq!(
            group(&report, "legacy.bin").completed_bytes,
            download::bytes_completed_on_disk(
                &temp.path().join("legacy.bin.part"),
                &temp.path().join("legacy.bin.part.json")
            )
        );
    }

    #[test]
    fn vanished_sidecar_falls_back_to_sampled_part_length() {
        let temp = tempfile::TempDir::new().unwrap();
        let sidecar_path = temp.path().join("vanished.bin.part.json");
        write_v1(sidecar_path.clone(), 100);
        let sidecar = FileSample {
            metadata: fs::metadata(&sidecar_path).unwrap(),
            path: sidecar_path.clone(),
        };
        fs::remove_file(sidecar_path).unwrap();

        assert_eq!(sidecar_progress(&sidecar, Some(40)), (None, Some(40)));
    }

    #[test]
    fn clean_skips_active_locks_and_deletes_inactive_groups() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("active.bin.part"), b"active").unwrap();
        let active_lock = temp.path().join("active.bin.part.json.lock");
        fs::write(&active_lock, b"").unwrap();
        fs::write(temp.path().join("stale.bin.part"), b"stale").unwrap();
        fs::write(temp.path().join("stale.bin.part.json.lock"), b"").unwrap();
        fs::write(temp.path().join("stale.bin.part.json.tmp"), b"tmp").unwrap();

        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&active_lock)
            .unwrap();
        FileExt::try_lock_exclusive(&lock_file).unwrap();

        let report = list(temp.path().to_owned()).unwrap();
        let clean_report = clean(temp.path().to_owned(), &report.groups, None).unwrap();

        assert_eq!(clean_report.skipped_active.len(), 1);
        assert!(temp.path().join("active.bin.part").exists());
        assert!(!temp.path().join("stale.bin.part").exists());
        assert!(!temp.path().join("stale.bin.part.json.tmp").exists());

        FileExt::unlock(&lock_file).unwrap();
    }

    #[test]
    fn clean_reprobes_lock_acquired_after_listing() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("late.bin.part"), b"late").unwrap();
        let lock_path = temp.path().join("late.bin.part.json.lock");
        fs::write(&lock_path, b"").unwrap();

        // Listing happens while the lock is free, so the snapshot says inactive.
        let report = list(temp.path().to_owned()).unwrap();
        assert!(!group(&report, "late.bin").active);

        // A download starts between the listing and the clean (the user may sit
        // at the confirmation prompt for a long time).
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        FileExt::try_lock_exclusive(&lock_file).unwrap();

        let clean_report = clean(temp.path().to_owned(), &report.groups, None).unwrap();

        assert!(clean_report.deleted.is_empty());
        assert_eq!(clean_report.skipped_active.len(), 1);
        assert!(temp.path().join("late.bin.part").exists());
        assert!(lock_path.exists());

        FileExt::unlock(&lock_file).unwrap();
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn list_and_clean_preserve_non_utf8_target_names() {
        let temp = tempfile::TempDir::new().unwrap();
        let target_name = OsString::from_vec(b"report-\x80.bin".to_vec());
        let target_path = temp.path().join(&target_name);
        let part_path = path_with_suffix(&target_path, ".part");
        let sidecar_path = path_with_suffix(&target_path, ".part.json");
        fs::write(&part_path, b"partial").unwrap();
        write_v1(sidecar_path, 20);

        let report = list(temp.path().to_owned()).unwrap();

        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].target_path, target_path);
        assert_eq!(
            report.groups[0].part_path.as_deref(),
            Some(part_path.as_path())
        );
        assert!(serde_json::to_string(&report).is_ok());

        let clean_report = clean(temp.path().to_owned(), &report.groups, None).unwrap();
        assert_eq!(clean_report.deleted.len(), 1);
        assert!(!part_path.exists());
        assert!(!path_with_suffix(&target_path, ".part.json").exists());
    }

    fn write_v1(path: PathBuf, expected: u64) {
        fs::write(
            path,
            serde_json::json!({
                "version": 1,
                "file_id": "0123abcd-000000example",
                "expected": expected,
                "key_used": false
            })
            .to_string(),
        )
        .unwrap();
    }

    fn write_v2(path: PathBuf, expected: u64, downloaded: u64) {
        fs::write(
            path,
            serde_json::json!({
                "version": 2,
                "file_id": "0123abcd-000000example",
                "expected": expected,
                "key_used": false,
                "segments": [
                    {"start": 0, "end": expected - 1, "done": false, "downloaded": downloaded}
                ]
            })
            .to_string(),
        )
        .unwrap();
    }

    fn group<'a>(report: &'a PartsReport, name: &str) -> &'a PartGroup {
        report
            .groups
            .iter()
            .find(|group| group.target_name == name)
            .unwrap()
    }
}
