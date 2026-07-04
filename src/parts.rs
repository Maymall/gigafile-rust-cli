// SPDX-License-Identifier: GPL-3.0-only

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::Serialize;
use serde_json::Value;

use crate::{
    download,
    error::{GfileError, IoOp},
};

#[derive(Debug, Clone, Serialize)]
pub struct PartsReport {
    pub status: &'static str,
    pub dir: PathBuf,
    pub groups: Vec<PartGroup>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PartGroup {
    pub target_name: String,
    pub target_path: PathBuf,
    pub part_path: Option<PathBuf>,
    pub sidecar_path: Option<PathBuf>,
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
    pub dir: PathBuf,
    pub deleted: Vec<CleanedGroup>,
    pub skipped_active: Vec<PartGroup>,
    pub failed: Vec<CleanFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanedGroup {
    pub target_name: String,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanFailure {
    pub target_name: String,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
struct GroupBuilder {
    part_path: Option<PathBuf>,
    sidecar_path: Option<PathBuf>,
    lock_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum PartFileKind {
    Part,
    Sidecar,
    Lock,
}

pub fn list(dir: PathBuf) -> Result<PartsReport, GfileError> {
    let mut builders = BTreeMap::<String, GroupBuilder>::new();
    for entry in fs::read_dir(&dir).map_err(|source| io_error(source, &dir, IoOp::Read))? {
        let entry = entry.map_err(|source| io_error(source, &dir, IoOp::Read))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((target_name, kind)) = classify_part_file(file_name) else {
            continue;
        };
        let builder = builders.entry(target_name).or_default();
        match kind {
            PartFileKind::Part => builder.part_path = Some(path),
            PartFileKind::Sidecar => builder.sidecar_path = Some(path),
            PartFileKind::Lock => builder.lock_path = Some(path),
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
        if group.active {
            skipped_active.push(group.clone());
            continue;
        }
        if !matches_older_than(group, older_than) {
            continue;
        }

        let mut removed_paths = Vec::new();
        for path in group_paths(group) {
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

    Ok(CleanReport {
        status: "ok",
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
    target_name: String,
    builder: GroupBuilder,
) -> Result<PartGroup, GfileError> {
    let target_path = dir.join(&target_name);
    let state = match (
        &builder.part_path,
        &builder.sidecar_path,
        &builder.lock_path,
    ) {
        (Some(_), Some(_), _) => PartState::Resumable,
        (Some(_), None, _) => PartState::PartWithoutSidecar,
        (None, Some(_), _) => PartState::SidecarWithoutPart,
        (None, None, Some(_)) => PartState::LockOnly,
        (None, None, None) => PartState::LockOnly,
    };
    let active = builder
        .lock_path
        .as_ref()
        .is_some_and(|path| lock_is_active(path).unwrap_or(false));
    let expected_bytes = builder
        .sidecar_path
        .as_ref()
        .and_then(|path| sidecar_expected(path));
    let completed_bytes = builder.part_path.as_ref().map(|part_path| {
        let fallback_sidecar = target_path.with_file_name(format!("{target_name}.part.json"));
        let sidecar_path = builder.sidecar_path.as_ref().unwrap_or(&fallback_sidecar);
        download::bytes_completed_on_disk(part_path, sidecar_path).unwrap_or(0)
    });
    let progress_percent = completed_bytes
        .zip(expected_bytes)
        .and_then(|(completed, expected)| {
            (expected > 0).then_some(completed as f64 / expected as f64 * 100.0)
        });
    let paths = [
        builder.part_path.as_ref(),
        builder.sidecar_path.as_ref(),
        builder.lock_path.as_ref(),
    ];
    let disk_bytes = paths
        .iter()
        .filter_map(|path| {
            path.and_then(|path| fs::metadata(path).ok())
                .map(|meta| meta.len())
        })
        .sum();
    let mtime_unix = paths
        .iter()
        .filter_map(|path| {
            path.and_then(|path| fs::metadata(path).ok())
                .and_then(|meta| meta.modified().ok())
                .and_then(system_time_unix)
        })
        .max();

    Ok(PartGroup {
        target_name,
        target_path,
        part_path: builder.part_path,
        sidecar_path: builder.sidecar_path,
        lock_path: builder.lock_path,
        state,
        active,
        disk_bytes,
        completed_bytes,
        expected_bytes,
        progress_percent,
        mtime_unix,
    })
}

fn classify_part_file(file_name: &str) -> Option<(String, PartFileKind)> {
    if let Some(target) = file_name.strip_suffix(".part.json.lock") {
        return (!target.is_empty()).then(|| (target.to_owned(), PartFileKind::Lock));
    }
    if let Some(target) = file_name.strip_suffix(".part.json") {
        return (!target.is_empty()).then(|| (target.to_owned(), PartFileKind::Sidecar));
    }
    if let Some(target) = file_name.strip_suffix(".part") {
        return (!target.is_empty()).then(|| (target.to_owned(), PartFileKind::Part));
    }
    None
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

fn sidecar_expected(path: &Path) -> Option<u64> {
    let value = serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok()?;
    match value.get("version")?.as_u64()? {
        1 => value.get("expected")?.as_u64(),
        2 => value.get("expected")?.as_u64(),
        _ => None,
    }
}

fn group_paths(group: &PartGroup) -> Vec<PathBuf> {
    [
        group.part_path.clone(),
        group.sidecar_path.clone(),
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
    fn list_groups_part_sidecar_and_lock_states() {
        let temp = tempfile::TempDir::new().unwrap();
        write_v1(temp.path().join("seq.bin.part.json"), 100);
        fs::write(temp.path().join("seq.bin.part"), vec![0_u8; 40]).unwrap();
        fs::write(temp.path().join("orphan.bin.part"), vec![0_u8; 5]).unwrap();
        write_v2(temp.path().join("seg.bin.part.json"), 200, 50);
        fs::write(temp.path().join("seg.bin.part"), vec![0_u8; 200]).unwrap();
        fs::write(temp.path().join("old.bin.part.json.lock"), b"").unwrap();

        let report = list(temp.path().to_owned()).unwrap();

        assert_eq!(report.groups.len(), 4);
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
    }

    #[test]
    fn clean_skips_active_locks_and_deletes_inactive_groups() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("active.bin.part"), b"active").unwrap();
        let active_lock = temp.path().join("active.bin.part.json.lock");
        fs::write(&active_lock, b"").unwrap();
        fs::write(temp.path().join("stale.bin.part"), b"stale").unwrap();
        fs::write(temp.path().join("stale.bin.part.json.lock"), b"").unwrap();

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

        FileExt::unlock(&lock_file).unwrap();
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
