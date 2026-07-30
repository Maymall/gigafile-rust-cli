// SPDX-License-Identifier: MIT

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    fs::{File as StdFile, OpenOptions as StdOpenOptions},
    io::{self, IsTerminal, Read as _},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use bytes::Bytes;
use fs2::FileExt;
use futures_util::stream::{FuturesUnordered, StreamExt};
use regex::Regex;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt, BufWriter, SeekFrom},
};
use tracing::{debug, info, warn};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as _;

use crate::{
    error::{GfileError, IoOp, boxed, internal_error, io_error, network_error},
    fsutil, http,
    jsonout::{self, ErrorJson},
    naming::{escape_terminal_text, log_name_diagnostics, sanitize_server_filename},
    parser::download::{
        PageInfo, PageKind, PageState, RemoteFile, classify_page, parse_download_page,
    },
    progress::{ByteProgress, SegmentProgressSpec, SegmentedProgress},
    urlinfo::parse_download_url,
};

pub const DEFAULT_DOWNLOAD_THREADS: u8 = 1;
pub const MIN_DOWNLOAD_THREADS: u8 = 1;
pub const MAX_DOWNLOAD_THREADS: u8 = 16;
const THREADS_RESUME_HINT: &str = "This often happens when a previous attempt used a different --threads value; rerun with the same --threads to resume, or accept the restart.";
// LT-9 (2026-07-04): short live probes did not show a stable throughput gain
// above four simultaneous Range streams, while prior user traces showed retry
// storms at eight. Keep the user-requested segment count for resume geometry,
// but only admit a conservative number of active segment requests at once.
const MAX_ACTIVE_SEGMENT_WORKERS: usize = 4;
const MIN_ADAPTIVE_SEGMENT_WORKERS: usize = 2;
const SEGMENT_SUCCESSES_BEFORE_PROBE: usize = 2;
const MAX_SELECTION_ITEMS: usize = 10_000;
// Persisting a complete JSON sidecar for every HTTP chunk can turn a large
// download into a metadata-heavy workload (and blocks the Tokio worker). A
// checkpoint is conservative: a crash may re-fetch at most this window.
const SEGMENT_CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SIDECAR_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub url: String,
    pub output: Option<PathBuf>,
    pub force: bool,
    pub no_resume: bool,
    pub key: Option<String>,
    pub selection: Option<FileSelection>,
    pub threads: u8,
    pub timeout: Duration,
    pub retries: u32,
    pub user_agent: Option<String>,
    pub dump_page: Option<PathBuf>,
    pub quiet: bool,
    pub allow_any_host: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSelection {
    indexes: Vec<usize>,
}

impl FileSelection {
    pub fn parse(spec: &str) -> Result<Self, GfileError> {
        let mut indexes = BTreeSet::new();
        for raw_part in spec.split(',') {
            let part = raw_part.trim();
            if part.is_empty() {
                return Err(selection_usage("empty selection item"));
            }
            if let Some((start, end)) = part.split_once('-') {
                let start = parse_selection_index(start.trim())?;
                let end = parse_selection_index(end.trim())?;
                if start > end {
                    return Err(selection_usage("range start is greater than range end"));
                }
                let count = end
                    .checked_sub(start)
                    .and_then(|count| count.checked_add(1))
                    .ok_or_else(|| selection_usage("selection range is too large"))?;
                if count > MAX_SELECTION_ITEMS
                    || indexes.len().saturating_add(count) > MAX_SELECTION_ITEMS
                {
                    return Err(selection_usage("selection range is too large"));
                }
                indexes.extend(start..=end);
            } else {
                indexes.insert(parse_selection_index(part)?);
            }
        }
        if indexes.is_empty() {
            return Err(selection_usage("selection is empty"));
        }
        if indexes.len() > MAX_SELECTION_ITEMS {
            return Err(selection_usage("selection contains too many indexes"));
        }
        Ok(Self {
            indexes: indexes.into_iter().collect(),
        })
    }

    fn contains(&self, index: usize) -> bool {
        self.indexes.binary_search(&index).is_ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadReport {
    pub kind: PageKind,
    pub files: Vec<DownloadFileRecord>,
    pub failed: usize,
    pub first_error: Option<ErrorJson>,
}

impl DownloadReport {
    pub fn first_failure_exit_code(&self) -> Option<u8> {
        self.first_error.as_ref().map(|error| error.exit_code)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadFileRecord {
    pub name: String,
    pub path: Option<PathBuf>,
    pub bytes: Option<u64>,
    pub resumed: bool,
    pub threads: Option<u8>,
    pub error: Option<ErrorJson>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SingleDownloadOutcome {
    name: Option<String>,
    path: PathBuf,
    bytes: u64,
    resumed: bool,
    threads: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum HttpValidator {
    StrongEtag(String),
    LastModified(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct PartSidecar {
    version: u8,
    file_id: String,
    expected: Option<u64>,
    key_used: bool,
    #[serde(default)]
    validator: Option<HttpValidator>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentSidecar {
    version: u8,
    file_id: String,
    expected: u64,
    key_used: bool,
    #[serde(default)]
    validator: Option<HttpValidator>,
    segments: Vec<SegmentState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SegmentState {
    start: u64,
    end: u64,
    done: bool,
    #[serde(default)]
    downloaded: u64,
}

#[derive(Debug, Clone)]
struct SegmentResumePlan {
    part_path: PathBuf,
    sidecar_path: PathBuf,
    expected: u64,
    validator: Option<HttpValidator>,
    segments: Vec<SegmentState>,
    resumed: bool,
}

struct SegmentedDownloadPlan {
    header_name: Option<String>,
    resume: SegmentResumePlan,
    initial: InitialSegmentResponse,
}

struct InitialSegmentResponse {
    index: usize,
    range_start: u64,
    response: reqwest::Response,
}

struct SequentialDownloadPlan {
    response: reqwest::Response,
    target_path: PathBuf,
    header_output_dir: Option<PathBuf>,
    resume: ResumePlan,
}

#[derive(Debug, Clone)]
struct ResumePlan {
    part_path: PathBuf,
    sidecar_path: PathBuf,
    range_start: Option<u64>,
    expected: Option<u64>,
    validator: Option<HttpValidator>,
}

#[derive(Debug, Clone, Copy)]
struct TransferPlan {
    append: bool,
    initial_bytes: u64,
    expected_total: Option<u64>,
    resumed: bool,
}

#[derive(Debug, Clone, Copy)]
enum PartOpenMode {
    CreateOrTruncate,
    AppendExisting,
    WriteExisting,
}

#[derive(Debug, Clone, Copy)]
struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

#[derive(Debug)]
enum SegmentDownloadError {
    Fallback(String),
    Failed(GfileError),
}

#[derive(Clone)]
struct SegmentContext {
    client: reqwest::Client,
    download_url: String,
    part_path: PathBuf,
    sidecar_path: PathBuf,
    file_id: String,
    expected: u64,
    key_used: bool,
    validator: Option<HttpValidator>,
    timeout: Duration,
    progress: SegmentedProgress,
    shared_segments: Arc<Mutex<Vec<SegmentState>>>,
}

struct SegmentWork {
    index: usize,
    attempt: u32,
    initial: Option<InitialSegmentWork>,
}

struct ScheduledSegmentWork {
    ready_at: tokio::time::Instant,
    work: SegmentWork,
}

struct InitialSegmentWork {
    range_start: u64,
    response: reqwest::Response,
}

struct SegmentWorkResult {
    index: usize,
    attempt: u32,
    result: Result<(), SegmentDownloadError>,
}

struct BatchTargetRegistry {
    state: Option<Mutex<BatchTargetState>>,
}

struct BatchTargetState {
    initially_present: HashSet<PathBuf>,
    claimed: HashMap<PathBuf, String>,
}

struct DownloadLock {
    file: StdFile,
    path: PathBuf,
}

impl DownloadLock {
    fn acquire(final_path: &Path) -> Result<Self, GfileError> {
        let (_, sidecar_path) = part_paths(final_path)?;
        let lock_path = lock_path_for_sidecar(&sidecar_path)?;
        let file = StdOpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| io_error(source, &lock_path, IoOp::Create))?;

        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Self {
                file,
                path: lock_path,
            }),
            Err(source) if is_lock_contention(&source) => {
                Err(GfileError::TargetLocked { path: lock_path })
            }
            Err(source) => Err(io_error(source, &lock_path, IoOp::Write)),
        }
    }
}

impl Drop for DownloadLock {
    fn drop(&mut self) {
        if let Err(source) = FileExt::unlock(&self.file) {
            warn!(path = ?self.path, %source, "failed to release download lock");
        }
    }
}

impl BatchTargetRegistry {
    fn for_page(kind: PageKind, output: Option<&Path>) -> Result<Self, GfileError> {
        if kind != PageKind::Matomete {
            return Ok(Self { state: None });
        }

        let output_dir = match output {
            Some(path) => path.to_owned(),
            None => std::env::current_dir()
                .map_err(|source| io_error(source, Path::new("."), IoOp::Metadata))?,
        };
        let entries = std::fs::read_dir(&output_dir)
            .map_err(|source| io_error(source, &output_dir, IoOp::Read))?;
        let mut initially_present = HashSet::new();
        for entry in entries {
            let entry = entry.map_err(|source| io_error(source, &output_dir, IoOp::Read))?;
            initially_present.insert(entry.path());
        }

        Ok(Self {
            state: Some(Mutex::new(BatchTargetState {
                initially_present,
                claimed: HashMap::new(),
            })),
        })
    }

    fn check_available(&self, path: &Path, file_id: &str, force: bool) -> Result<(), GfileError> {
        let Some(state) = &self.state else {
            return ensure_unclaimed_target_available(path, force);
        };
        let state = state
            .lock()
            .map_err(|_| internal_error("batch target registry lock was poisoned"))?;
        if state
            .claimed
            .get(path)
            .is_some_and(|owner| owner != file_id)
            || (path.exists() && (!force || !state.initially_present.contains(path)))
        {
            return Err(target_exists(path));
        }
        Ok(())
    }

    fn claim(&self, path: &Path, file_id: &str, force: bool) -> Result<(), GfileError> {
        let Some(state) = &self.state else {
            return ensure_unclaimed_target_available(path, force);
        };
        let mut state = state
            .lock()
            .map_err(|_| internal_error("batch target registry lock was poisoned"))?;
        if let Some(owner) = state.claimed.get(path) {
            if owner == file_id {
                return Ok(());
            }
            return Err(target_exists(path));
        }
        if path.exists() && (!force || !state.initially_present.contains(path)) {
            return Err(target_exists(path));
        }
        state.claimed.insert(path.to_owned(), file_id.to_owned());
        Ok(())
    }

    fn may_replace_existing(&self, path: &Path, force: bool) -> Result<bool, GfileError> {
        if !force {
            return Ok(false);
        }
        let Some(state) = &self.state else {
            return Ok(true);
        };
        let state = state
            .lock()
            .map_err(|_| internal_error("batch target registry lock was poisoned"))?;
        Ok(state.initially_present.contains(path))
    }
}

pub fn validate_threads(threads: u8) -> Result<u8, GfileError> {
    if (MIN_DOWNLOAD_THREADS..=MAX_DOWNLOAD_THREADS).contains(&threads) {
        Ok(threads)
    } else {
        Err(GfileError::Usage {
            message: format!(
                "download threads must be between {MIN_DOWNLOAD_THREADS} and {MAX_DOWNLOAD_THREADS}, got {threads}"
            ),
        })
    }
}

pub async fn download(mut options: DownloadOptions) -> Result<DownloadReport, GfileError> {
    let url_info = parse_download_url(&options.url, options.allow_any_host)?;
    let client =
        http::build_gigafile_client(options.user_agent.as_deref(), options.allow_any_host)?;

    let page_response = http::get_with_retries_and_timeout(
        &client,
        &url_info.page_url,
        options.retries,
        "fetching page",
        Some(options.timeout),
    )
    .await?;
    let page_status = page_response.status().as_u16();
    let final_page_url = page_response.url().clone();
    let page_bytes = http::read_body_limited(
        page_response,
        http::PAGE_BODY_LIMIT,
        options.timeout,
        "reading download page body",
    )
    .await?;

    if let Some(path) = &options.dump_page {
        fs::write(path, &page_bytes)
            .await
            .map_err(|source| io_error(source, path, IoOp::Write))?;
        eprintln!("Warning: dumped page may contain private filenames; do not share it publicly.");
    }

    let html = String::from_utf8_lossy(&page_bytes);
    if redirected_to_gigafile_home(&final_page_url) {
        return Err(GfileError::NotFoundOrExpired);
    }
    let state = classify_page(&html, page_status);
    match state {
        PageState::Ok => {}
        PageState::NeedsKey => {
            if options.key.is_none() {
                options.key = Some(prompt_or_require_key()?);
            }
        }
        PageState::WrongKey => return Err(GfileError::KeyWrong),
        PageState::NotFoundOrExpired => return Err(GfileError::NotFoundOrExpired),
        PageState::Unknown => {
            return Err(GfileError::Parse {
                what: "download page state is unknown".to_owned(),
                hint: "Page structure may have changed; rerun with --dump-page and -vv.".to_owned(),
            });
        }
    }

    let page = parse_download_page(&html, &url_info.file_id)?;
    validate_selection(&page, options.selection.as_ref())?;
    validate_output_for_page(&page, options.output.as_deref())?;
    let batch_targets = BatchTargetRegistry::for_page(page.kind, options.output.as_deref())?;

    let selected_files = selected_files(&page, options.selection.as_ref());
    let mut records = Vec::with_capacity(selected_files.len());
    let mut first_error = None;
    for remote_file in selected_files {
        let final_path =
            resolve_output_path(remote_file, page.kind, options.output.as_deref()).await?;
        if let Err(error) =
            batch_targets.check_available(&final_path, &remote_file.file_id, options.force)
        {
            if page.kind == PageKind::Single {
                return Err(error);
            }
            record_error(
                &mut records,
                &mut first_error,
                remote_file,
                Some(final_path),
                &error,
            );
            continue;
        }

        let sanitized_name = sanitize_server_filename(&remote_file.raw_name, &remote_file.file_id);
        log_name_diagnostics(&remote_file.raw_name, &sanitized_name, &final_path);
        let download_url = url_info.download_url_for(&remote_file.file_id, options.key.as_deref());

        match download_file_with_retries(
            &client,
            &download_url,
            remote_file,
            &final_path,
            &options,
            &batch_targets,
        )
        .await
        {
            Ok(outcome) => records.push(DownloadFileRecord {
                name: outcome.name.unwrap_or_else(|| remote_file.raw_name.clone()),
                path: Some(outcome.path),
                bytes: Some(outcome.bytes),
                resumed: outcome.resumed,
                threads: Some(outcome.threads),
                error: None,
            }),
            Err(error) if page.kind == PageKind::Single => return Err(error),
            Err(error) => {
                record_error(
                    &mut records,
                    &mut first_error,
                    remote_file,
                    Some(final_path),
                    &error,
                );
            }
        }
    }

    let failed = records
        .iter()
        .filter(|record| record.error.is_some())
        .count();
    Ok(DownloadReport {
        kind: page.kind,
        files: records,
        failed,
        first_error,
    })
}

fn parse_selection_index(value: &str) -> Result<usize, GfileError> {
    let index = value
        .parse::<usize>()
        .map_err(|_| selection_usage("selection entries must be positive integers"))?;
    if index == 0 {
        return Err(selection_usage("selection indexes start at 1"));
    }
    Ok(index)
}

fn selection_usage(detail: &str) -> GfileError {
    GfileError::Usage {
        message: format!("invalid --select value: {detail}; use `rgfile info` to see file numbers"),
    }
}

fn validate_selection(
    page: &PageInfo,
    selection: Option<&FileSelection>,
) -> Result<(), GfileError> {
    let Some(selection) = selection else {
        return Ok(());
    };
    if page.kind == PageKind::Single {
        if selection.indexes.as_slice() == [1] {
            return Ok(());
        }
        return Err(GfileError::Usage {
            message:
                "single-file pages only accept --select 1; use `rgfile info` to see file numbers"
                    .to_owned(),
        });
    }
    let max = page.files.len();
    if let Some(index) = selection.indexes.iter().find(|index| **index > max) {
        return Err(GfileError::Usage {
            message: format!(
                "selection index {index} is out of range for {max} files; use `rgfile info` to see file numbers"
            ),
        });
    }
    Ok(())
}

fn selected_files<'a>(
    page: &'a PageInfo,
    selection: Option<&FileSelection>,
) -> Vec<&'a RemoteFile> {
    page.files
        .iter()
        .enumerate()
        .filter_map(|(offset, file)| {
            let index = offset + 1;
            if selection.is_none_or(|selection| selection.contains(index)) {
                Some(file)
            } else {
                None
            }
        })
        .collect()
}

fn record_error(
    records: &mut Vec<DownloadFileRecord>,
    first_error: &mut Option<ErrorJson>,
    remote_file: &RemoteFile,
    path: Option<PathBuf>,
    error: &GfileError,
) {
    let json_error = jsonout::error_json(error);
    if first_error.is_none() {
        *first_error = Some(json_error.clone());
    }
    records.push(DownloadFileRecord {
        name: remote_file.raw_name.clone(),
        path,
        bytes: None,
        resumed: false,
        threads: None,
        error: Some(json_error),
    });
}

async fn download_file_with_retries(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, GfileError> {
    let _download_lock = DownloadLock::acquire(final_path)?;
    let mut attempt = 0;
    loop {
        match try_download_file(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await
        {
            Ok(outcome) => return Ok(outcome),
            Err(error) if http::is_retryable(&error) && attempt < options.retries => {
                warn!(
                    "retrying file download after error: {}",
                    error.user_message()
                );
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn try_download_file(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, GfileError> {
    if options.threads > DEFAULT_DOWNLOAD_THREADS {
        return try_download_file_segmented_or_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }

    try_download_file_sequential(
        client,
        download_url,
        remote_file,
        final_path,
        options,
        batch_targets,
    )
    .await
}

async fn try_download_file_sequential(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, GfileError> {
    let target_path = final_path.to_owned();
    let header_output_dir = header_filename_output_dir(final_path, options.output.as_deref())?;
    let mut resume = prepare_resume(&target_path, remote_file, options).await?;
    let mut response = send_download_request(
        client,
        download_url,
        resume.range_start,
        resume.validator.as_ref(),
        options,
    )
    .await?;

    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
        batch_targets.claim(&target_path, &remote_file.file_id, options.force)?;
        let replace_existing = batch_targets.may_replace_existing(&target_path, options.force)?;
        if let Some(outcome) =
            complete_if_range_already_finished(&response, &resume, &target_path, replace_existing)
                .await?
        {
            return Ok(outcome);
        }
        warn!("server did not confirm the completed resume range; restarting from zero");
        remove_if_exists(&resume.part_path).await?;
        remove_if_exists(&resume.sidecar_path).await?;
        resume.range_start = None;
        resume.expected = None;
        resume.validator = None;
        response = send_download_request(client, download_url, None, None, options).await?;
    }
    restart_sequential_if_validator_mismatch(
        client,
        download_url,
        &mut response,
        &mut resume,
        options,
    )
    .await?;

    consume_download_response_sequential(
        client,
        download_url,
        remote_file,
        SequentialDownloadPlan {
            response,
            target_path,
            header_output_dir,
            resume,
        },
        options,
        batch_targets,
    )
    .await
}

async fn consume_download_response_sequential(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    plan: SequentialDownloadPlan,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, GfileError> {
    let SequentialDownloadPlan {
        mut response,
        mut target_path,
        header_output_dir,
        mut resume,
    } = plan;

    if !response.status().is_success() {
        return Err(http::status_error(response.status(), download_url));
    }

    if is_html_content_type(response.headers()) {
        let body = http::read_body_limited(
            response,
            http::PAGE_BODY_LIMIT,
            options.timeout,
            "reading HTML download error body",
        )
        .await?;
        return Err(classify_html_response(
            &String::from_utf8_lossy(&body),
            options.key.is_some(),
            "download response content-type is HTML",
        ));
    }

    let header_name = content_disposition_filename(response.headers());
    let _header_target_lock = if resume.range_start.is_none() {
        if let (Some(dir), Some(name)) = (header_output_dir.as_deref(), header_name.as_deref()) {
            let header_path = dir.join(sanitize_server_filename(name, &remote_file.file_id));
            if header_path != target_path {
                batch_targets.check_available(&header_path, &remote_file.file_id, options.force)?;
                let lock = DownloadLock::acquire(&header_path)?;
                let header_resume = prepare_resume(&header_path, remote_file, options).await?;
                let should_retry_with_header_resume = header_resume.range_start.is_some();
                target_path = header_path;
                resume = header_resume;
                batch_targets.claim(&target_path, &remote_file.file_id, options.force)?;
                if should_retry_with_header_resume {
                    response = send_download_request(
                        client,
                        download_url,
                        resume.range_start,
                        resume.validator.as_ref(),
                        options,
                    )
                    .await?;
                    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                        let replace_existing =
                            batch_targets.may_replace_existing(&target_path, options.force)?;
                        if let Some(outcome) = complete_if_range_already_finished(
                            &response,
                            &resume,
                            &target_path,
                            replace_existing,
                        )
                        .await?
                        {
                            return Ok(outcome);
                        }
                        warn!(
                            "server did not confirm the completed resume range; restarting from zero"
                        );
                        remove_if_exists(&resume.part_path).await?;
                        remove_if_exists(&resume.sidecar_path).await?;
                        resume.range_start = None;
                        resume.expected = None;
                        resume.validator = None;
                        response = send_download_request(client, download_url, None, None, options)
                            .await?;
                    }
                    restart_sequential_if_validator_mismatch(
                        client,
                        download_url,
                        &mut response,
                        &mut resume,
                        options,
                    )
                    .await?;
                }
                Some(lock)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    batch_targets.claim(&target_path, &remote_file.file_id, options.force)?;

    if !response.status().is_success() {
        return Err(http::status_error(response.status(), download_url));
    }

    if is_html_content_type(response.headers()) {
        let body = http::read_body_limited(
            response,
            http::PAGE_BODY_LIMIT,
            options.timeout,
            "reading HTML download error body",
        )
        .await?;
        return Err(classify_html_response(
            &String::from_utf8_lossy(&body),
            options.key.is_some(),
            "download response content-type is HTML",
        ));
    }

    let transfer = transfer_plan(&response, &resume)?;
    if transfer.expected_total.is_none() {
        warn!("download response has no Content-Length; exact size check is disabled");
    }
    warn_on_display_size_mismatch(remote_file, transfer.expected_total);

    let first_chunk = match next_chunk(&mut response, options.timeout).await {
        Ok(chunk) => chunk,
        Err(ChunkReadError::Timeout) => {
            return Err(timeout_network_error("reading first download chunk"));
        }
        Err(ChunkReadError::Http(source)) => {
            return Err(network_error(source, "reading first download chunk"));
        }
    };

    if content_type_is_missing(response.headers())
        && let Some(chunk) = first_chunk.as_deref()
        && let Some(error) = classify_ambiguous_body_probe(chunk, options.key.is_some())
    {
        return Err(error);
    }

    write_sidecar(
        &resume.sidecar_path,
        remote_file,
        transfer.expected_total,
        options.key.is_some(),
        validator_from_headers(response.headers()),
    )?;
    crate::interrupt::set_active_download(Some(crate::interrupt::ActiveDownload {
        part_path: resume.part_path.clone(),
        sidecar_path: resume.sidecar_path.clone(),
        expected: transfer.expected_total,
    }));

    let file = if transfer.append {
        open_part_file(&resume.part_path, PartOpenMode::AppendExisting)
            .await
            .map_err(|source| io_error(source, &resume.part_path, IoOp::Write))?
    } else {
        open_part_file(&resume.part_path, PartOpenMode::CreateOrTruncate)
            .await
            .map_err(|source| io_error(source, &resume.part_path, IoOp::Create))?
    };
    let mut writer = BufWriter::with_capacity(256 * 1024, file);
    let progress = ByteProgress::new(
        transfer.expected_total,
        options.quiet,
        &progress_label(&target_path, &remote_file.raw_name),
    );
    if transfer.initial_bytes > 0 {
        progress.inc(transfer.initial_bytes);
    }
    let mut actual = transfer.initial_bytes;

    if let Some(chunk) = first_chunk {
        writer
            .write_all(&chunk)
            .await
            .map_err(|source| io_error(source, &resume.part_path, IoOp::Write))?;
        actual += chunk.len() as u64;
        progress.inc(chunk.len() as u64);
    }

    loop {
        let chunk = match next_chunk(&mut response, options.timeout).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(ChunkReadError::Timeout) => {
                flush_before_return(&mut writer).await;
                return Err(timeout_network_error("reading download chunk"));
            }
            Err(ChunkReadError::Http(source))
                if transfer.expected_total.is_some() && source.is_decode() =>
            {
                flush_before_return(&mut writer).await;
                return Err(GfileError::SizeMismatch {
                    expected: transfer.expected_total.unwrap(),
                    actual,
                });
            }
            Err(ChunkReadError::Http(source)) => {
                flush_before_return(&mut writer).await;
                return Err(network_error(source, "reading download chunk"));
            }
        };
        writer
            .write_all(&chunk)
            .await
            .map_err(|source| io_error(source, &resume.part_path, IoOp::Write))?;
        actual += chunk.len() as u64;
        progress.inc(chunk.len() as u64);
    }
    progress.finish();

    if let Some(expected) = transfer.expected_total
        && actual != expected
    {
        writer
            .flush()
            .await
            .map_err(|source| io_error(source, &resume.part_path, IoOp::Write))?;
        return Err(GfileError::SizeMismatch { expected, actual });
    }

    writer
        .flush()
        .await
        .map_err(|source| io_error(source, &resume.part_path, IoOp::Write))?;
    drop(writer);

    let replace_existing = batch_targets.may_replace_existing(&target_path, options.force)?;
    promote_part(
        &resume.part_path,
        &resume.sidecar_path,
        &target_path,
        replace_existing,
    )
    .await?;

    info!(bytes = actual, path = ?target_path, "download complete");

    Ok(SingleDownloadOutcome {
        name: header_name,
        path: target_path,
        bytes: actual,
        resumed: transfer.resumed,
        threads: DEFAULT_DOWNLOAD_THREADS,
    })
}

async fn try_download_file_segmented_or_fallback(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, GfileError> {
    if let Some(segment_resume) =
        load_existing_segmented_resume(final_path, remote_file, options).await?
    {
        return try_download_file_segmented_resume(
            client,
            download_url,
            remote_file,
            final_path,
            segment_resume,
            options,
            batch_targets,
        )
        .await;
    }

    try_download_file_segmented_fresh(
        client,
        download_url,
        remote_file,
        final_path,
        options,
        batch_targets,
    )
    .await
}

async fn try_download_file_segmented_fresh(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, GfileError> {
    let range_end = initial_segment_end(remote_file, options.threads);
    let response =
        send_range_request(client, download_url, 0, range_end, None, options.timeout).await?;
    if response.status() == StatusCode::OK {
        warn!(
            "segmented download was not accepted by the server: server returned HTTP 200 to the first Range request; consuming this response with one connection"
        );
        return consume_200_fallback_response(
            response,
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }
    if !response.status().is_success() {
        return Err(http::status_error(response.status(), download_url));
    }
    if response.status() != StatusCode::PARTIAL_CONTENT {
        warn!(
            "segmented download was not accepted by the server: server returned HTTP {} to the first Range request; falling back to one connection",
            response.status().as_u16()
        );
        return sequential_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }

    if is_html_content_type(response.headers()) {
        let body = http::read_body_limited(
            response,
            http::PAGE_BODY_LIMIT,
            options.timeout,
            "reading HTML download error body",
        )
        .await?;
        return Err(classify_html_response(
            &String::from_utf8_lossy(&body),
            options.key.is_some(),
            "download response content-type is HTML",
        ));
    }

    let content_range = parse_content_range(response.headers())?;
    let reached_eof = content_range
        .total
        .is_some_and(|total| content_range.end.saturating_add(1) == total);
    if content_range.start != 0 || (content_range.end != range_end && !reached_eof) {
        warn!(
            "segmented download was not accepted by the server: Content-Range was {}-{}, expected 0-{range_end}; falling back to one connection",
            content_range.start, content_range.end
        );
        return sequential_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }
    let Some(expected) = content_range.total else {
        warn!(
            "segmented download response has no Content-Range total; falling back to one connection"
        );
        return sequential_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    };
    if expected == 0 {
        warn!("empty file download uses the single-connection path");
        return sequential_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }
    warn_on_display_size_mismatch(remote_file, Some(expected));

    let validator = validator_from_headers(response.headers());

    let header_name = content_disposition_filename(response.headers());
    let header_output_dir = header_filename_output_dir(final_path, options.output.as_deref())?;
    let mut target_path = final_path.to_owned();
    let _header_target_lock =
        if let (Some(dir), Some(name)) = (header_output_dir.as_deref(), header_name.as_deref()) {
            let header_path = dir.join(sanitize_server_filename(name, &remote_file.file_id));
            if header_path != target_path {
                batch_targets.check_available(&header_path, &remote_file.file_id, options.force)?;
                let lock = DownloadLock::acquire(&header_path)?;
                target_path = header_path;
                Some(lock)
            } else {
                None
            }
        } else {
            None
        };
    batch_targets.claim(&target_path, &remote_file.file_id, options.force)?;

    if let Some(segment_resume) =
        load_existing_segmented_resume(&target_path, remote_file, options).await?
    {
        // The probe response was only needed to discover Content-Disposition.
        // Keep resumed writes on the existing resume path to avoid truncating the true-name .part.
        return try_download_file_segmented_resume(
            client,
            download_url,
            remote_file,
            &target_path,
            segment_resume,
            options,
            batch_targets,
        )
        .await;
    }

    let (part_path, sidecar_path) = part_paths(&target_path)?;
    let segments = build_segments_from_initial(expected, options.threads, content_range.end);
    let segment_resume = SegmentResumePlan {
        part_path,
        sidecar_path,
        expected,
        validator,
        segments,
        resumed: false,
    };
    let part_path = segment_resume.part_path.clone();
    let sidecar_path = segment_resume.sidecar_path.clone();

    match try_download_file_segmented(
        client,
        download_url,
        remote_file,
        &target_path,
        SegmentedDownloadPlan {
            header_name,
            resume: segment_resume,
            initial: InitialSegmentResponse {
                index: 0,
                range_start: 0,
                response,
            },
        },
        options,
        batch_targets,
    )
    .await
    {
        Ok(outcome) => Ok(outcome),
        Err(SegmentDownloadError::Fallback(reason)) => {
            warn!(reason = ?reason, "segmented download was not accepted; falling back to one connection");
            remove_if_exists(&part_path).await?;
            remove_if_exists(&sidecar_path).await?;
            sequential_fallback(
                client,
                download_url,
                remote_file,
                &target_path,
                options,
                batch_targets,
            )
            .await
        }
        Err(SegmentDownloadError::Failed(error)) => Err(error),
    }
}

async fn try_download_file_segmented_resume(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    segment_resume: SegmentResumePlan,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, GfileError> {
    batch_targets.claim(final_path, &remote_file.file_id, options.force)?;
    let Some((index, segment)) = first_incomplete_segment(&segment_resume.segments) else {
        let replace_existing = batch_targets.may_replace_existing(final_path, options.force)?;
        promote_part(
            &segment_resume.part_path,
            &segment_resume.sidecar_path,
            final_path,
            replace_existing,
        )
        .await?;
        return Ok(SingleDownloadOutcome {
            name: None,
            path: final_path.to_owned(),
            bytes: segment_resume.expected,
            resumed: true,
            threads: segment_resume.segments.len() as u8,
        });
    };
    let range_start = segment.start + segment.downloaded.min(segment_len(&segment));
    let response = send_range_request(
        client,
        download_url,
        range_start,
        segment.end,
        segment_resume.validator.as_ref(),
        options.timeout,
    )
    .await?;
    if response.status() == StatusCode::OK {
        warn!(
            "segmented download was not accepted by the server: server returned HTTP 200 to a resumed Range request; clearing segments and consuming this response with one connection"
        );
        remove_if_exists(&segment_resume.part_path).await?;
        remove_if_exists(&segment_resume.sidecar_path).await?;
        return consume_200_fallback_response(
            response,
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }
    if !response.status().is_success() {
        return Err(http::status_error(response.status(), download_url));
    }
    if response.status() != StatusCode::PARTIAL_CONTENT {
        warn!(
            "segmented download was not accepted by the server: server returned HTTP {} to a resumed Range request; falling back to one connection",
            response.status().as_u16()
        );
        remove_if_exists(&segment_resume.part_path).await?;
        remove_if_exists(&segment_resume.sidecar_path).await?;
        return sequential_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }

    if !validator_matches_headers(segment_resume.validator.as_ref(), response.headers()) {
        warn!("segmented resume response validator changed; clearing segments and restarting");
        remove_if_exists(&segment_resume.part_path).await?;
        remove_if_exists(&segment_resume.sidecar_path).await?;
        return sequential_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }

    let content_range = parse_content_range(response.headers())?;
    if content_range.start != range_start || content_range.end != segment.end {
        warn!(
            start = content_range.start,
            end = content_range.end,
            expected_start = range_start,
            expected_end = segment.end,
            "segmented download was not accepted; Content-Range mismatch"
        );
        remove_if_exists(&segment_resume.part_path).await?;
        remove_if_exists(&segment_resume.sidecar_path).await?;
        return sequential_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }
    if let Some(total) = content_range.total
        && total != segment_resume.expected
    {
        warn!(
            "segmented download was not accepted by the server: Content-Range total is {total}, expected {}; falling back to one connection",
            segment_resume.expected
        );
        remove_if_exists(&segment_resume.part_path).await?;
        remove_if_exists(&segment_resume.sidecar_path).await?;
        return sequential_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
            batch_targets,
        )
        .await;
    }

    let header_name = content_disposition_filename(response.headers());
    let part_path = segment_resume.part_path.clone();
    let sidecar_path = segment_resume.sidecar_path.clone();
    match try_download_file_segmented(
        client,
        download_url,
        remote_file,
        final_path,
        SegmentedDownloadPlan {
            header_name,
            resume: segment_resume,
            initial: InitialSegmentResponse {
                index,
                range_start,
                response,
            },
        },
        options,
        batch_targets,
    )
    .await
    {
        Ok(outcome) => Ok(outcome),
        Err(SegmentDownloadError::Fallback(reason)) => {
            warn!(reason = ?reason, "segmented download was not accepted; falling back to one connection");
            remove_if_exists(&part_path).await?;
            remove_if_exists(&sidecar_path).await?;
            sequential_fallback(
                client,
                download_url,
                remote_file,
                final_path,
                options,
                batch_targets,
            )
            .await
        }
        Err(SegmentDownloadError::Failed(error)) => Err(error),
    }
}

async fn consume_200_fallback_response(
    response: reqwest::Response,
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, GfileError> {
    let mut sequential_options = options.clone();
    sequential_options.threads = DEFAULT_DOWNLOAD_THREADS;
    sequential_options.no_resume = true;
    let header_output_dir =
        header_filename_output_dir(final_path, sequential_options.output.as_deref())?;
    let resume = fresh_resume_plan(final_path)?;
    consume_download_response_sequential(
        client,
        download_url,
        remote_file,
        SequentialDownloadPlan {
            response,
            target_path: final_path.to_owned(),
            header_output_dir,
            resume,
        },
        &sequential_options,
        batch_targets,
    )
    .await
}

async fn sequential_fallback(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, GfileError> {
    let mut sequential_options = options.clone();
    sequential_options.threads = DEFAULT_DOWNLOAD_THREADS;
    sequential_options.no_resume = true;
    try_download_file_sequential(
        client,
        download_url,
        remote_file,
        final_path,
        &sequential_options,
        batch_targets,
    )
    .await
}

async fn try_download_file_segmented(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    plan: SegmentedDownloadPlan,
    options: &DownloadOptions,
    batch_targets: &BatchTargetRegistry,
) -> Result<SingleDownloadOutcome, SegmentDownloadError> {
    let SegmentedDownloadPlan {
        header_name,
        resume: segment_resume,
        initial,
    } = plan;
    let expected = segment_resume.expected;
    let part_file = if segment_resume.resumed {
        open_part_file(&segment_resume.part_path, PartOpenMode::WriteExisting)
            .await
            .map_err(|source| {
                SegmentDownloadError::Failed(io_error(
                    source,
                    &segment_resume.part_path,
                    IoOp::Write,
                ))
            })?
    } else {
        open_part_file(&segment_resume.part_path, PartOpenMode::CreateOrTruncate)
            .await
            .map_err(|source| {
                SegmentDownloadError::Failed(io_error(
                    source,
                    &segment_resume.part_path,
                    IoOp::Create,
                ))
            })?
    };
    part_file.set_len(expected).await.map_err(|source| {
        SegmentDownloadError::Failed(io_error(source, &segment_resume.part_path, IoOp::Write))
    })?;
    drop(part_file);

    write_segment_sidecar(
        &segment_resume.sidecar_path,
        &remote_file.file_id,
        expected,
        options.key.is_some(),
        segment_resume.validator.as_ref(),
        &segment_resume.segments,
    )
    .map_err(SegmentDownloadError::Failed)?;
    crate::interrupt::set_active_download(Some(crate::interrupt::ActiveDownload {
        part_path: segment_resume.part_path.clone(),
        sidecar_path: segment_resume.sidecar_path.clone(),
        expected: Some(expected),
    }));

    let segment_progress = segment_resume
        .segments
        .iter()
        .map(|segment| SegmentProgressSpec {
            len: segment_len(segment),
            initial: segment_completed_bytes(segment),
        })
        .collect::<Vec<_>>();
    let progress = SegmentedProgress::new(
        Some(expected),
        options.quiet,
        &progress_label(final_path, &remote_file.raw_name),
        &segment_progress,
    );

    let shared_segments = Arc::new(Mutex::new(segment_resume.segments.clone()));
    let context = SegmentContext {
        client: client.clone(),
        download_url: download_url.to_owned(),
        part_path: segment_resume.part_path.clone(),
        sidecar_path: segment_resume.sidecar_path.clone(),
        file_id: remote_file.file_id.clone(),
        expected,
        key_used: options.key.is_some(),
        validator: segment_resume.validator.clone(),
        timeout: options.timeout,
        progress: progress.clone(),
        shared_segments: Arc::clone(&shared_segments),
    };
    let initial_limit = active_segment_limit(segment_resume.segments.len());
    let pending = pending_segment_work(&segment_resume.segments, initial);
    if let Some(error) =
        run_segment_scheduler(context.clone(), pending, options.retries, initial_limit).await
    {
        return Err(error);
    }
    progress.finish();

    let final_segments =
        segment_snapshot(&shared_segments).map_err(SegmentDownloadError::Failed)?;
    if final_segments.iter().any(|segment| !segment.done) {
        return Err(SegmentDownloadError::Failed(GfileError::SizeMismatch {
            expected,
            actual: final_segments.iter().map(segment_completed_bytes).sum(),
        }));
    }

    write_segment_sidecar(
        &segment_resume.sidecar_path,
        &remote_file.file_id,
        expected,
        options.key.is_some(),
        segment_resume.validator.as_ref(),
        &final_segments,
    )
    .map_err(SegmentDownloadError::Failed)?;

    let replace_existing = batch_targets
        .may_replace_existing(final_path, options.force)
        .map_err(SegmentDownloadError::Failed)?;
    promote_part(
        &segment_resume.part_path,
        &segment_resume.sidecar_path,
        final_path,
        replace_existing,
    )
    .await
    .map_err(SegmentDownloadError::Failed)?;

    info!(bytes = expected, path = ?final_path, "download complete");

    Ok(SingleDownloadOutcome {
        name: header_name,
        path: final_path.to_owned(),
        bytes: expected,
        resumed: segment_resume.resumed,
        threads: final_segments.len() as u8,
    })
}

fn active_segment_limit(segment_count: usize) -> usize {
    segment_count
        .min(MAX_ACTIVE_SEGMENT_WORKERS)
        .max(DEFAULT_DOWNLOAD_THREADS as usize)
}

fn pending_segment_work(
    segments: &[SegmentState],
    initial: InitialSegmentResponse,
) -> VecDeque<SegmentWork> {
    let mut pending = VecDeque::with_capacity(segments.len());
    let initial_index = initial.index;
    pending.push_back(SegmentWork {
        index: initial.index,
        attempt: 0,
        initial: Some(InitialSegmentWork {
            range_start: initial.range_start,
            response: initial.response,
        }),
    });
    for (index, segment) in segments.iter().enumerate() {
        if index == initial_index || segment.done {
            continue;
        }
        pending.push_back(SegmentWork {
            index,
            attempt: 0,
            initial: None,
        });
    }
    pending
}

async fn run_segment_scheduler(
    context: SegmentContext,
    mut pending: VecDeque<SegmentWork>,
    retries: u32,
    initial_limit: usize,
) -> Option<SegmentDownloadError> {
    let mut active = FuturesUnordered::new();
    let mut delayed = VecDeque::<ScheduledSegmentWork>::new();
    let mut active_limit = initial_limit;
    let mut first_error = None;
    let mut consecutive_successes = 0_usize;

    for work in &pending {
        mark_segment_waiting(&context, work.index);
    }

    loop {
        let now = tokio::time::Instant::now();
        while delayed.front().is_some_and(|work| work.ready_at <= now) {
            let scheduled = delayed
                .pop_front()
                .expect("front was checked before popping delayed work");
            pending.push_back(scheduled.work);
        }

        while active.len() < active_limit {
            let Some(work) = pending.pop_front() else {
                break;
            };
            mark_segment_active(&context, work.index);
            active.push(run_segment_work(context.clone(), work));
        }

        if active.is_empty() {
            let Some(next_retry) = delayed.front() else {
                break;
            };
            tokio::time::sleep_until(next_retry.ready_at).await;
            continue;
        }

        let next_result = if active.len() < active_limit {
            if let Some(next_retry) = delayed.front() {
                tokio::select! {
                    result = active.next() => result,
                    () = tokio::time::sleep_until(next_retry.ready_at) => continue,
                }
            } else {
                active.next().await
            }
        } else {
            active.next().await
        };
        let Some(work_result) = next_result else {
            break;
        };
        match work_result.result {
            Ok(()) => {
                consecutive_successes += 1;
                if active_limit < initial_limit
                    && consecutive_successes >= SEGMENT_SUCCESSES_BEFORE_PROBE
                {
                    active_limit += 1;
                    consecutive_successes = 0;
                }
            }
            Err(SegmentDownloadError::Fallback(reason)) => {
                return Some(SegmentDownloadError::Fallback(reason));
            }
            Err(SegmentDownloadError::Failed(error))
                if http::is_retryable(&error) && work_result.attempt < retries =>
            {
                warn!(
                    "retrying download segment {} after error: {}",
                    work_result.index + 1,
                    error.user_message()
                );
                consecutive_successes = 0;
                if active_limit > MIN_ADAPTIVE_SEGMENT_WORKERS {
                    active_limit = (active_limit / 2).max(MIN_ADAPTIVE_SEGMENT_WORKERS);
                }
                mark_segment_waiting(&context, work_result.index);
                schedule_segment_work(
                    &mut delayed,
                    ScheduledSegmentWork {
                        ready_at: tokio::time::Instant::now()
                            + http::retry_delay(work_result.attempt),
                        work: SegmentWork {
                            index: work_result.index,
                            attempt: work_result.attempt + 1,
                            initial: None,
                        },
                    },
                );
            }
            Err(error) => {
                consecutive_successes = 0;
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    first_error
}

fn schedule_segment_work(delayed: &mut VecDeque<ScheduledSegmentWork>, work: ScheduledSegmentWork) {
    let position = delayed
        .iter()
        .position(|queued| queued.ready_at > work.ready_at)
        .unwrap_or(delayed.len());
    delayed.insert(position, work);
}

async fn run_segment_work(context: SegmentContext, work: SegmentWork) -> SegmentWorkResult {
    mark_segment_active(&context, work.index);
    let result = match work.initial {
        Some(initial) => {
            consume_segment_response(&context, work.index, initial.range_start, initial.response)
                .await
        }
        None => try_download_segment(&context, work.index).await,
    };
    SegmentWorkResult {
        index: work.index,
        attempt: work.attempt,
        result,
    }
}

fn mark_segment_waiting(context: &SegmentContext, index: usize) {
    context
        .progress
        .set_segment_message(index, format!("conn {} waiting", index + 1));
}

fn mark_segment_active(context: &SegmentContext, index: usize) {
    context
        .progress
        .set_segment_message(index, format!("conn {}", index + 1));
}

async fn try_download_segment(
    context: &SegmentContext,
    index: usize,
) -> Result<(), SegmentDownloadError> {
    let segment =
        segment_at(&context.shared_segments, index).map_err(SegmentDownloadError::Failed)?;
    let segment_len = segment_len(&segment);
    let already_downloaded = segment.downloaded.min(segment_len);
    // A failed attempt may have written bytes after the last durable
    // checkpoint. The retry overwrites that range, so reset the visible count
    // instead of counting those bytes twice.
    context
        .progress
        .set_segment_position(index, already_downloaded);
    if segment.done || already_downloaded == segment_len {
        update_segment_sidecar_sync(context, index, segment_len, true)
            .map_err(SegmentDownloadError::Failed)?;
        return Ok(());
    }

    let range_start = segment.start + already_downloaded;
    let response = send_segment_request(context, range_start, segment.end).await?;
    consume_segment_response(context, index, range_start, response).await
}

async fn consume_segment_response(
    context: &SegmentContext,
    index: usize,
    range_start: u64,
    mut response: reqwest::Response,
) -> Result<(), SegmentDownloadError> {
    let segment =
        segment_at(&context.shared_segments, index).map_err(SegmentDownloadError::Failed)?;
    let segment_len = segment_len(&segment);
    let already_downloaded = range_start.saturating_sub(segment.start);
    if response.status() == StatusCode::OK {
        return Err(SegmentDownloadError::Fallback(
            "server returned HTTP 200 to a Range request".to_owned(),
        ));
    }
    if !response.status().is_success() {
        return Err(SegmentDownloadError::Failed(http::status_error(
            response.status(),
            &context.download_url,
        )));
    }
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(SegmentDownloadError::Fallback(format!(
            "server returned HTTP {} to a Range request",
            response.status().as_u16()
        )));
    }
    if !validator_matches_headers(context.validator.as_ref(), response.headers()) {
        return Err(SegmentDownloadError::Fallback(
            "segment response validator changed".to_owned(),
        ));
    }

    let content_range = parse_content_range(response.headers()).map_err(|error| {
        SegmentDownloadError::Fallback(format!(
            "invalid segment Content-Range: {}",
            error.user_message()
        ))
    })?;
    if content_range.start != range_start {
        return Err(SegmentDownloadError::Fallback(format!(
            "Content-Range starts at {}, expected {}",
            content_range.start, range_start
        )));
    }
    if content_range.end != segment.end {
        return Err(SegmentDownloadError::Fallback(format!(
            "Content-Range ends at {}, expected {}",
            content_range.end, segment.end
        )));
    }
    if let Some(total) = content_range.total
        && total != context.expected
    {
        return Err(SegmentDownloadError::Fallback(format!(
            "Content-Range total is {total}, expected {}",
            context.expected
        )));
    }

    let mut file = open_part_file(&context.part_path, PartOpenMode::WriteExisting)
        .await
        .map_err(|source| {
            SegmentDownloadError::Failed(io_error(source, &context.part_path, IoOp::Write))
        })?;
    file.seek(SeekFrom::Start(range_start))
        .await
        .map_err(|source| {
            SegmentDownloadError::Failed(io_error(source, &context.part_path, IoOp::Write))
        })?;

    let mut downloaded = already_downloaded;
    let mut persisted_downloaded = already_downloaded;
    loop {
        let chunk = match next_chunk(&mut response, context.timeout).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(ChunkReadError::Timeout) => {
                return Err(SegmentDownloadError::Failed(timeout_network_error(
                    "reading download segment",
                )));
            }
            Err(ChunkReadError::Http(source)) => {
                return Err(SegmentDownloadError::Failed(network_error(
                    source,
                    "reading download segment",
                )));
            }
        };
        if downloaded + chunk.len() as u64 > segment_len {
            return Err(SegmentDownloadError::Failed(GfileError::SizeMismatch {
                expected: segment_len,
                actual: downloaded + chunk.len() as u64,
            }));
        }
        file.write_all(&chunk).await.map_err(|source| {
            SegmentDownloadError::Failed(io_error(source, &context.part_path, IoOp::Write))
        })?;
        downloaded += chunk.len() as u64;
        context.progress.inc(index, chunk.len() as u64);
        if downloaded.saturating_sub(persisted_downloaded) >= SEGMENT_CHECKPOINT_BYTES {
            file.sync_data().await.map_err(|source| {
                SegmentDownloadError::Failed(io_error(source, &context.part_path, IoOp::Write))
            })?;
            update_segment_sidecar_sync(context, index, downloaded, false)
                .map_err(SegmentDownloadError::Failed)?;
            persisted_downloaded = downloaded;
        }
    }

    if downloaded != segment_len {
        return Err(SegmentDownloadError::Failed(GfileError::SizeMismatch {
            expected: segment_len,
            actual: downloaded,
        }));
    }
    file.sync_data().await.map_err(|source| {
        SegmentDownloadError::Failed(io_error(source, &context.part_path, IoOp::Write))
    })?;
    update_segment_sidecar_sync(context, index, downloaded, true)
        .map_err(SegmentDownloadError::Failed)?;
    Ok(())
}

async fn send_segment_request(
    context: &SegmentContext,
    start: u64,
    end: u64,
) -> Result<reqwest::Response, SegmentDownloadError> {
    send_range_request(
        &context.client,
        &context.download_url,
        start,
        end,
        context.validator.as_ref(),
        context.timeout,
    )
    .await
    .map_err(SegmentDownloadError::Failed)
}

async fn send_range_request(
    client: &reqwest::Client,
    download_url: &str,
    start: u64,
    end: u64,
    validator: Option<&HttpValidator>,
    timeout: Duration,
) -> Result<reqwest::Response, GfileError> {
    let mut request = client
        .get(download_url)
        .header(header::RANGE, format!("bytes={start}-{end}"));
    if let Some(validator) = validator {
        request = request.header(header::IF_RANGE, validator_header_value(validator));
    }
    let result = tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| timeout_network_error("starting download segment"))?;
    result.map_err(|source| network_error(source, "starting download segment"))
}

async fn load_existing_segmented_resume(
    final_path: &Path,
    remote_file: &RemoteFile,
    options: &DownloadOptions,
) -> Result<Option<SegmentResumePlan>, GfileError> {
    let (part_path, sidecar_path) = part_paths(final_path)?;
    if options.no_resume {
        remove_if_exists(&part_path).await?;
        remove_if_exists(&sidecar_path).await?;
        return Ok(None);
    }

    if !part_path.exists() {
        debug!(path = ?part_path, "looked for existing segmented .part, not found");
        return Ok(None);
    }

    let sidecar = match read_sidecar_limited(&sidecar_path) {
        Ok(bytes) => parse_segment_sidecar(&bytes),
        Err(_) => None,
    };
    let Some(mut sidecar) = sidecar else {
        warn!(
            "existing segmented .part has missing or damaged v2 sidecar; restarting from zero. {THREADS_RESUME_HINT}"
        );
        return Ok(None);
    };

    if sidecar.version != 2
        || sidecar.file_id != remote_file.file_id
        || sidecar.key_used != options.key.is_some()
        || sidecar
            .validator
            .as_ref()
            .is_none_or(|validator| !validator_is_valid(validator))
        || !normalize_segments(sidecar.expected, &mut sidecar.segments)
    {
        warn!(
            "existing .part sidecar cannot be used for this segmented download; restarting from zero. {THREADS_RESUME_HINT}"
        );
        return Ok(None);
    }

    let part_len = fs::metadata(&part_path)
        .await
        .map_err(|source| io_error(source, &part_path, IoOp::Metadata))?
        .len();
    if part_len != sidecar.expected {
        warn!(
            "existing segmented .part length is {part_len}, expected {}; restarting from zero. {THREADS_RESUME_HINT}",
            sidecar.expected
        );
        return Ok(None);
    }

    let resumed = sidecar
        .segments
        .iter()
        .any(|segment| segment.done || segment.downloaded > 0);
    Ok(Some(SegmentResumePlan {
        part_path,
        sidecar_path,
        expected: sidecar.expected,
        validator: sidecar.validator,
        segments: sidecar.segments,
        resumed,
    }))
}

fn build_segments_from_initial(
    expected: u64,
    requested_threads: u8,
    initial_end: u64,
) -> Vec<SegmentState> {
    let first_end = initial_end.min(expected.saturating_sub(1));
    let mut segments = vec![SegmentState {
        start: 0,
        end: first_end,
        done: false,
        downloaded: 0,
    }];
    let mut start = first_end + 1;
    if start >= expected {
        return segments;
    }

    let remaining_threads = u64::from(requested_threads)
        .saturating_sub(1)
        .min(expected - start)
        .max(1);
    let remaining = expected - start;
    let base = remaining / remaining_threads;
    let remainder = remaining % remaining_threads;
    for index in 0..remaining_threads {
        let len = base + u64::from(index < remainder);
        let end = start + len - 1;
        segments.push(SegmentState {
            start,
            end,
            done: false,
            downloaded: 0,
        });
        start = end + 1;
    }
    segments
}

fn initial_segment_end(remote_file: &RemoteFile, requested_threads: u8) -> u64 {
    let approx = remote_file.approx_bytes.unwrap_or(1);
    let segment_len = (approx / u64::from(requested_threads)).max(1);
    segment_len - 1
}

fn first_incomplete_segment(segments: &[SegmentState]) -> Option<(usize, SegmentState)> {
    segments
        .iter()
        .enumerate()
        .find(|(_, segment)| !segment.done && segment.downloaded < segment_len(segment))
        .map(|(index, segment)| (index, segment.clone()))
}

fn parse_segment_sidecar(bytes: &[u8]) -> Option<SegmentSidecar> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if value.get("version")?.as_u64()? != 2 {
        return None;
    }
    serde_json::from_value(value).ok()
}

fn normalize_segments(expected: u64, segments: &mut [SegmentState]) -> bool {
    if expected == 0 || segments.is_empty() || segments.len() > usize::from(MAX_DOWNLOAD_THREADS) {
        return false;
    }

    let mut expected_start = 0;
    for segment in segments {
        if segment.start != expected_start || segment.end < segment.start || segment.end >= expected
        {
            return false;
        }
        let Some(len) = segment
            .end
            .checked_sub(segment.start)
            .and_then(|len| len.checked_add(1))
        else {
            return false;
        };
        if segment.downloaded > len {
            return false;
        }
        if segment.done {
            segment.downloaded = len;
        } else if segment.downloaded == len {
            segment.done = true;
        }
        expected_start = match segment.end.checked_add(1) {
            Some(start) => start,
            None => return false,
        };
    }
    expected_start == expected
}

fn segment_at(
    segments: &Arc<Mutex<Vec<SegmentState>>>,
    index: usize,
) -> Result<SegmentState, GfileError> {
    let guard = segments
        .lock()
        .map_err(|_| internal_error("segmented download state lock was poisoned".to_owned()))?;
    guard
        .get(index)
        .cloned()
        .ok_or_else(|| internal_error(format!("missing segment state at index {index}")))
}

fn segment_snapshot(
    segments: &Arc<Mutex<Vec<SegmentState>>>,
) -> Result<Vec<SegmentState>, GfileError> {
    segments
        .lock()
        .map(|segments| segments.clone())
        .map_err(|_| internal_error("segmented download state lock was poisoned".to_owned()))
}

fn update_segment_sidecar_sync(
    context: &SegmentContext,
    index: usize,
    downloaded: u64,
    done: bool,
) -> Result<(), GfileError> {
    let mut segments = context
        .shared_segments
        .lock()
        .map_err(|_| internal_error("segmented download state lock was poisoned".to_owned()))?;
    let Some(segment) = segments.get_mut(index) else {
        return Err(internal_error(format!(
            "missing segment state at index {index}"
        )));
    };
    segment.downloaded = downloaded;
    segment.done = done;
    let bytes = segment_sidecar_bytes(
        &context.file_id,
        context.expected,
        context.key_used,
        context.validator.as_ref(),
        &segments,
    )?;
    // Atomic replace: other code (the interrupt summary or a resume check) may
    // read the checkpoint at any moment;
    // a plain truncate-and-write leaves it unparsable for most of its life.
    // Writers are serialized by the shared_segments lock held above.
    fsutil::write_atomic(&context.sidecar_path, &bytes)
        .map_err(|source| io_error(source, &context.sidecar_path, IoOp::Write))
}

fn write_segment_sidecar(
    sidecar_path: &Path,
    file_id: &str,
    expected: u64,
    key_used: bool,
    validator: Option<&HttpValidator>,
    segments: &[SegmentState],
) -> Result<(), GfileError> {
    let sidecar_bytes = segment_sidecar_bytes(file_id, expected, key_used, validator, segments)?;
    fsutil::write_atomic(sidecar_path, &sidecar_bytes)
        .map_err(|source| io_error(source, sidecar_path, IoOp::Write))
}

fn segment_sidecar_bytes(
    file_id: &str,
    expected: u64,
    key_used: bool,
    validator: Option<&HttpValidator>,
    segments: &[SegmentState],
) -> Result<Vec<u8>, GfileError> {
    let sidecar = SegmentSidecar {
        version: 2,
        file_id: file_id.to_owned(),
        expected,
        key_used,
        validator: validator.cloned(),
        segments: segments.to_vec(),
    };
    serde_json::to_vec(&sidecar).map_err(|source| {
        internal_error(format!("failed to serialize segmented sidecar: {source}"))
    })
}

fn segment_len(segment: &SegmentState) -> u64 {
    segment.end - segment.start + 1
}

fn segment_completed_bytes(segment: &SegmentState) -> u64 {
    if segment.done {
        segment_len(segment)
    } else {
        segment.downloaded.min(segment_len(segment))
    }
}

async fn restart_sequential_if_validator_mismatch(
    client: &reqwest::Client,
    download_url: &str,
    response: &mut reqwest::Response,
    resume: &mut ResumePlan,
    options: &DownloadOptions,
) -> Result<(), GfileError> {
    if response.status() != StatusCode::PARTIAL_CONTENT
        || resume.range_start.is_none()
        || validator_matches_headers(resume.validator.as_ref(), response.headers())
    {
        return Ok(());
    }

    warn!("resume response validator changed; discarding partial data and restarting from zero");
    remove_if_exists(&resume.part_path).await?;
    remove_if_exists(&resume.sidecar_path).await?;
    resume.range_start = None;
    resume.expected = None;
    resume.validator = None;
    *response = send_download_request(client, download_url, None, None, options).await?;
    Ok(())
}

async fn send_download_request(
    client: &reqwest::Client,
    download_url: &str,
    range_start: Option<u64>,
    validator: Option<&HttpValidator>,
    options: &DownloadOptions,
) -> Result<reqwest::Response, GfileError> {
    let mut request = client.get(download_url);
    if let Some(start) = range_start {
        request = request.header(header::RANGE, format!("bytes={start}-"));
        if let Some(validator) = validator {
            request = request.header(header::IF_RANGE, validator_header_value(validator));
        }
    }
    let result = tokio::time::timeout(options.timeout, request.send())
        .await
        .map_err(|_| timeout_network_error("starting file download"))?;
    result.map_err(|source| network_error(source, "starting file download"))
}

async fn open_part_file(path: &Path, mode: PartOpenMode) -> io::Result<File> {
    let before = match fs::symlink_metadata(path).await {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    let mut options = OpenOptions::new();
    match mode {
        PartOpenMode::CreateOrTruncate if before.is_none() => {
            options.write(true).create_new(true);
        }
        PartOpenMode::CreateOrTruncate => {
            // Do not request O_TRUNC here. The path may be swapped between
            // symlink_metadata and open; truncating during open would damage
            // the replacement before its identity can be rejected below.
            options.write(true);
        }
        PartOpenMode::AppendExisting => {
            options.append(true);
        }
        PartOpenMode::WriteExisting => {
            options.write(true);
        }
    }
    configure_no_follow(&mut options);
    let file = options.open(path).await?;
    let opened = file.metadata().await?;
    if !is_safe_part_metadata(&opened) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "partial-download path is not a regular non-reparse file",
        ));
    }

    let after = fs::symlink_metadata(path).await?;
    if !is_safe_part_metadata(&after) || !same_file_identity(&opened, &after) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "partial-download path changed while it was opened",
        ));
    }
    if let Some(before) = before.as_ref()
        && (!is_safe_part_metadata(before) || !same_file_identity(before, &opened))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "partial-download path changed before it was opened",
        ));
    }
    if matches!(mode, PartOpenMode::CreateOrTruncate) {
        // Truncate only the validated handle. A later path replacement cannot
        // redirect this operation to another file.
        file.set_len(0).await?;
    }
    Ok(file)
}

fn configure_no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
}

fn is_safe_part_metadata(metadata: &std::fs::Metadata) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return false;
    }
    #[cfg(windows)]
    if metadata.file_attributes() & 0x400 != 0 {
        // FILE_ATTRIBUTE_REPARSE_POINT
        return false;
    }
    true
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    // Windows opens with FILE_FLAG_OPEN_REPARSE_POINT and rejects every
    // reparse-point handle above. Stable std does not expose file-index
    // identity, so the handle-level reparse check is the portable guarantee.
    true
}

fn validator_from_headers(headers: &header::HeaderMap) -> Option<HttpValidator> {
    strong_etag_from_headers(headers)
        .map(HttpValidator::StrongEtag)
        .or_else(|| last_modified_from_headers(headers).map(HttpValidator::LastModified))
}

fn validator_matches_headers(
    expected: Option<&HttpValidator>,
    headers: &header::HeaderMap,
) -> bool {
    match expected {
        Some(HttpValidator::StrongEtag(expected)) => {
            strong_etag_from_headers(headers).as_deref() == Some(expected)
        }
        Some(HttpValidator::LastModified(expected)) => {
            last_modified_from_headers(headers).as_deref() == Some(expected)
        }
        None => validator_from_headers(headers).is_none(),
    }
}

fn strong_etag_from_headers(headers: &header::HeaderMap) -> Option<String> {
    let value = headers.get(header::ETAG)?.to_str().ok()?.trim();
    valid_strong_etag(value).then(|| value.to_owned())
}

fn last_modified_from_headers(headers: &header::HeaderMap) -> Option<String> {
    let value = headers.get(header::LAST_MODIFIED)?.to_str().ok()?.trim();
    valid_last_modified(value).then(|| value.to_owned())
}

fn validator_header_value(validator: &HttpValidator) -> &str {
    match validator {
        HttpValidator::StrongEtag(value) | HttpValidator::LastModified(value) => value,
    }
}

fn validator_is_valid(validator: &HttpValidator) -> bool {
    match validator {
        HttpValidator::StrongEtag(value) => valid_strong_etag(value),
        HttpValidator::LastModified(value) => valid_last_modified(value),
    }
}

fn valid_strong_etag(value: &str) -> bool {
    value.trim() == value
        && value.len() >= 2
        && value.starts_with('"')
        && value.ends_with('"')
        && !value
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("w/"))
        && value.parse::<header::HeaderValue>().is_ok()
}

fn valid_last_modified(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && value.parse::<header::HeaderValue>().is_ok()
}

async fn prepare_resume(
    final_path: &Path,
    remote_file: &RemoteFile,
    options: &DownloadOptions,
) -> Result<ResumePlan, GfileError> {
    let (part_path, sidecar_path) = part_paths(final_path)?;
    if options.no_resume || !part_path.exists() {
        return Ok(ResumePlan {
            part_path,
            sidecar_path,
            range_start: None,
            expected: None,
            validator: None,
        });
    }

    let sidecar = match read_sidecar_limited(&sidecar_path) {
        Ok(bytes) => serde_json::from_slice::<PartSidecar>(&bytes).ok(),
        Err(_) => None,
    };
    let Some(sidecar) = sidecar else {
        warn!(
            "existing .part has missing or damaged sidecar; restarting from zero. {THREADS_RESUME_HINT}"
        );
        return Ok(ResumePlan {
            part_path,
            sidecar_path,
            range_start: None,
            expected: None,
            validator: None,
        });
    };

    if sidecar.version != 1
        || sidecar.file_id != remote_file.file_id
        || sidecar.expected.is_none()
        || sidecar.key_used != options.key.is_some()
        || sidecar
            .validator
            .as_ref()
            .is_none_or(|validator| !validator_is_valid(validator))
    {
        warn!(
            "existing .part sidecar does not match this file; restarting from zero. {THREADS_RESUME_HINT}"
        );
        return Ok(ResumePlan {
            part_path,
            sidecar_path,
            range_start: None,
            expected: None,
            validator: None,
        });
    }

    let len = fs::metadata(&part_path)
        .await
        .map_err(|source| io_error(source, &part_path, IoOp::Metadata))?
        .len();
    if len == 0 {
        return Ok(ResumePlan {
            part_path,
            sidecar_path,
            range_start: None,
            expected: sidecar.expected,
            validator: None,
        });
    }

    Ok(ResumePlan {
        part_path,
        sidecar_path,
        range_start: Some(len),
        expected: sidecar.expected,
        validator: sidecar.validator,
    })
}

fn transfer_plan(
    response: &reqwest::Response,
    resume: &ResumePlan,
) -> Result<TransferPlan, GfileError> {
    match response.status() {
        StatusCode::PARTIAL_CONTENT => {
            let expected_start = resume.range_start.ok_or_else(|| GfileError::Parse {
                what: "server returned 206 without a resume request".to_owned(),
                hint: "Retry the download from zero; if it repeats, report the response headers."
                    .to_owned(),
            })?;
            let content_range = parse_content_range(response.headers())?;
            if content_range.start != expected_start {
                return Err(GfileError::Parse {
                    what: format!(
                        "Content-Range starts at {}, expected {}",
                        content_range.start, expected_start
                    ),
                    hint: "The existing .part file may not match the remote file; retry with --no-resume."
                        .to_owned(),
                });
            }
            Ok(TransferPlan {
                append: true,
                initial_bytes: expected_start,
                expected_total: content_range.total.or(resume.expected),
                resumed: expected_start > 0,
            })
        }
        StatusCode::OK => {
            if resume.range_start.is_some() {
                info!("server ignored Range request; restarting this file from zero");
            }
            Ok(TransferPlan {
                append: false,
                initial_bytes: 0,
                expected_total: response.content_length(),
                resumed: false,
            })
        }
        _ => Err(http::status_error(response.status(), "")),
    }
}

async fn complete_if_range_already_finished(
    response: &reqwest::Response,
    resume: &ResumePlan,
    final_path: &Path,
    replace_existing: bool,
) -> Result<Option<SingleDownloadOutcome>, GfileError> {
    let Some(start) = resume.range_start else {
        return Ok(None);
    };
    if resume.expected != Some(start)
        || unsatisfied_content_range_total(response.headers()) != Some(start)
        || !validator_matches_headers(resume.validator.as_ref(), response.headers())
    {
        return Ok(None);
    }
    promote_part(
        &resume.part_path,
        &resume.sidecar_path,
        final_path,
        replace_existing,
    )
    .await?;
    Ok(Some(SingleDownloadOutcome {
        name: None,
        path: final_path.to_owned(),
        bytes: start,
        resumed: true,
        threads: DEFAULT_DOWNLOAD_THREADS,
    }))
}

fn write_sidecar(
    sidecar_path: &Path,
    remote_file: &RemoteFile,
    expected: Option<u64>,
    key_used: bool,
    validator: Option<HttpValidator>,
) -> Result<(), GfileError> {
    let sidecar = PartSidecar {
        version: 1,
        file_id: remote_file.file_id.clone(),
        expected,
        key_used,
        validator,
    };
    let sidecar_bytes = serde_json::to_vec(&sidecar)
        .map_err(|source| internal_error(format!("failed to serialize sidecar: {source}")))?;
    fsutil::write_atomic(sidecar_path, &sidecar_bytes)
        .map_err(|source| io_error(source, sidecar_path, IoOp::Write))
}

/// How many bytes of a `.part` are actually usable for resume, read back from
/// disk: the v2 sidecar's per-segment progress when present (the segmented
/// `.part` is preallocated to full size, so its length says nothing), the
/// `.part` length for the sequential v1 sidecar (append-only writes), and the
/// `.part` length when no sidecar exists at all.
///
/// Returns `None` when a sidecar exists but cannot be understood — reporting
/// the preallocated `.part` length in that case would claim 100% progress for
/// a barely-started segmented download.
pub(crate) fn bytes_completed_on_disk(part_path: &Path, sidecar_path: &Path) -> Option<u64> {
    for attempt in 0..3 {
        match read_sidecar_limited(sidecar_path) {
            Ok(bytes) => {
                if let Some(mut sidecar) = parse_segment_sidecar(&bytes) {
                    if std::fs::metadata(part_path).ok()?.len() != sidecar.expected
                        || !normalize_segments(sidecar.expected, &mut sidecar.segments)
                    {
                        return None;
                    }
                    return sidecar
                        .segments
                        .iter()
                        .try_fold(0_u64, |completed, segment| {
                            completed.checked_add(segment_completed_bytes(segment))
                        });
                }
                let version = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|value| value.get("version")?.as_u64());
                if version == Some(1) {
                    // Sequential downloads append, so the .part length is the progress.
                    return std::fs::metadata(part_path).ok().map(|meta| meta.len());
                }
                if version.is_none() && attempt < 2 {
                    // Possibly caught mid-write; give the writer a moment.
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                }
                return None;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return std::fs::metadata(part_path).ok().map(|meta| meta.len());
            }
            Err(_) => return None,
        }
    }
    None
}

pub(crate) fn read_sidecar_limited(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_SIDECAR_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "partial-download sidecar is too large or is not a regular file",
        ));
    }
    let file = StdFile::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_SIDECAR_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "partial-download sidecar exceeds the size limit",
        ));
    }
    Ok(bytes)
}

async fn promote_part(
    part_path: &Path,
    sidecar_path: &Path,
    final_path: &Path,
    force: bool,
) -> Result<(), GfileError> {
    // Every completion path, including a resumed HTTP 416 or an already
    // complete segmented sidecar, passes through this validation and sync.
    let part_file = open_part_file(part_path, PartOpenMode::WriteExisting)
        .await
        .map_err(|source| io_error(source, part_path, IoOp::Write))?;
    part_file
        .sync_all()
        .await
        .map_err(|source| io_error(source, part_path, IoOp::Write))?;
    drop(part_file);

    if force {
        // Both paths are created in the same directory, so replacement is an
        // atomic replacement on the supported platforms. Keeping the old
        // target in place until this succeeds preserves it on I/O failure.
        fsutil::replace_file(part_path, final_path)
            .map_err(|source| io_error(source, final_path, IoOp::Rename))?;
    } else {
        fsutil::move_file_noreplace(part_path, final_path).map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                GfileError::TargetExists {
                    path: final_path.to_owned(),
                }
            } else {
                io_error(source, final_path, IoOp::Rename)
            }
        })?;
    }

    if let Err(error) = remove_if_exists(sidecar_path).await {
        warn!(
            path = ?sidecar_path,
            error = %error.user_message(),
            "download committed but partial-download sidecar cleanup failed"
        );
    }
    if let Err(source) = sync_parent_directory(final_path) {
        warn!(
            path = ?final_path,
            %source,
            "download committed but syncing the target directory failed"
        );
    }
    crate::interrupt::set_active_download(None);
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    StdFile::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

async fn remove_if_exists(path: &Path) -> Result<(), GfileError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(source, path, IoOp::Write)),
    }
}

async fn next_chunk(
    response: &mut reqwest::Response,
    timeout: Duration,
) -> Result<Option<Bytes>, ChunkReadError> {
    match tokio::time::timeout(timeout, response.chunk()).await {
        Ok(Ok(Some(chunk))) => Ok(Some(chunk)),
        Ok(Err(source)) => Err(ChunkReadError::Http(source)),
        Ok(Ok(None)) => Ok(None),
        Err(_) => Err(ChunkReadError::Timeout),
    }
}

enum ChunkReadError {
    Http(reqwest::Error),
    Timeout,
}

async fn resolve_output_path(
    remote_file: &RemoteFile,
    kind: PageKind,
    output: Option<&Path>,
) -> Result<PathBuf, GfileError> {
    match output {
        Some(path) if path.exists() && path.is_dir() => {
            let name = sanitize_server_filename(&remote_file.raw_name, &remote_file.file_id);
            Ok(path.join(name))
        }
        Some(path) if kind == PageKind::Single => Ok(path.to_owned()),
        Some(path) => Err(GfileError::Usage {
            message: format!(
                "matomete downloads require --output to be an existing directory, got {}",
                path.display()
            ),
        }),
        None => {
            let name = sanitize_server_filename(&remote_file.raw_name, &remote_file.file_id);
            std::env::current_dir()
                .map(|cwd| cwd.join(name))
                .map_err(|source| io_error(source, Path::new("."), IoOp::Metadata))
        }
    }
}

fn validate_output_for_page(page: &PageInfo, output: Option<&Path>) -> Result<(), GfileError> {
    if page.kind != PageKind::Matomete {
        return Ok(());
    }
    if let Some(path) = output
        && !(path.exists() && path.is_dir())
    {
        return Err(GfileError::Usage {
            message: format!(
                "matomete downloads require --output to be an existing directory, got {}",
                path.display()
            ),
        });
    }
    Ok(())
}

fn ensure_unclaimed_target_available(final_path: &Path, force: bool) -> Result<(), GfileError> {
    if final_path.exists() && !force {
        return Err(target_exists(final_path));
    }
    Ok(())
}

fn target_exists(path: &Path) -> GfileError {
    GfileError::TargetExists {
        path: path.to_owned(),
    }
}

fn part_paths(final_path: &Path) -> Result<(PathBuf, PathBuf), GfileError> {
    let file_name = final_path.file_name().ok_or_else(|| {
        io_error(
            io::Error::new(io::ErrorKind::InvalidInput, "target path has no filename"),
            final_path,
            IoOp::Create,
        )
    })?;
    let mut part_name = file_name.to_os_string();
    part_name.push(".part");
    let mut sidecar_name = part_name.clone();
    sidecar_name.push(".json");

    let mut part = final_path.to_owned();
    part.set_file_name(part_name);
    let mut sidecar = final_path.to_owned();
    sidecar.set_file_name(sidecar_name);
    Ok((part, sidecar))
}

fn lock_path_for_sidecar(sidecar_path: &Path) -> Result<PathBuf, GfileError> {
    let file_name = sidecar_path.file_name().ok_or_else(|| {
        io_error(
            io::Error::new(io::ErrorKind::InvalidInput, "sidecar path has no filename"),
            sidecar_path,
            IoOp::Create,
        )
    })?;
    let mut lock_path = sidecar_path.to_owned();
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    lock_path.set_file_name(lock_name);
    Ok(lock_path)
}

pub(crate) fn is_lock_contention(source: &io::Error) -> bool {
    source.kind() == io::ErrorKind::WouldBlock
        || matches!(
            source.raw_os_error(),
            // Unix EAGAIN/EWOULDBLOCK and Windows ERROR_SHARING_VIOLATION /
            // ERROR_LOCK_VIOLATION can surface from nonblocking file locks.
            Some(11 | 32 | 33 | 35)
        )
}

fn fresh_resume_plan(final_path: &Path) -> Result<ResumePlan, GfileError> {
    let (part_path, sidecar_path) = part_paths(final_path)?;
    Ok(ResumePlan {
        part_path,
        sidecar_path,
        range_start: None,
        expected: None,
        validator: None,
    })
}

// The page display name can be a server-side mask (e.g. `******.ext`); the
// resolved target path already carries the Content-Disposition name, so the
// progress bar must label with the target, not the page name.
fn progress_label(target_path: &Path, fallback: &str) -> String {
    target_path
        .file_name()
        .map(|name| escape_terminal_text(&name.to_string_lossy()).into_owned())
        .unwrap_or_else(|| escape_terminal_text(fallback).into_owned())
}

fn header_filename_output_dir(
    final_path: &Path,
    output: Option<&Path>,
) -> Result<Option<PathBuf>, GfileError> {
    match output {
        Some(path) if path.exists() && path.is_dir() => Ok(Some(path.to_owned())),
        Some(_) => Ok(None),
        None => final_path
            .parent()
            .map(|path| Some(path.to_owned()))
            .ok_or_else(|| {
                io_error(
                    io::Error::new(io::ErrorKind::InvalidInput, "target path has no parent"),
                    final_path,
                    IoOp::Create,
                )
            }),
    }
}

fn parse_content_range(headers: &header::HeaderMap) -> Result<ContentRange, GfileError> {
    let value = headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| GfileError::Parse {
            what: "206 response missing Content-Range".to_owned(),
            hint: "Retry with --no-resume; if it repeats, report the response headers.".to_owned(),
        })?;
    static CONTENT_RANGE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^bytes +(\d+)-(\d+)/(\d+|\*)$").expect("valid Content-Range parser regex")
    });
    let captures = CONTENT_RANGE_RE
        .captures(value)
        .ok_or_else(|| GfileError::Parse {
            what: format!("invalid Content-Range header {value:?}"),
            hint: "Retry with --no-resume; if it repeats, report the response headers.".to_owned(),
        })?;
    let start = captures[1].parse::<u64>().map_err(|_| GfileError::Parse {
        what: format!("invalid Content-Range start in {value:?}"),
        hint: "Retry with --no-resume; if it repeats, report the response headers.".to_owned(),
    })?;
    let end = captures[2].parse::<u64>().map_err(|_| GfileError::Parse {
        what: format!("invalid Content-Range end in {value:?}"),
        hint: "Retry with --no-resume; if it repeats, report the response headers.".to_owned(),
    })?;
    let total = if &captures[3] == "*" {
        None
    } else {
        Some(captures[3].parse::<u64>().map_err(|_| GfileError::Parse {
            what: format!("invalid Content-Range total in {value:?}"),
            hint: "Retry with --no-resume; if it repeats, report the response headers.".to_owned(),
        })?)
    };
    if end < start || total.is_some_and(|total| total == 0 || end >= total) {
        return Err(GfileError::Parse {
            what: format!("inconsistent Content-Range header {value:?}"),
            hint: "Retry with --no-resume; if it repeats, report the response headers.".to_owned(),
        });
    }
    Ok(ContentRange { start, end, total })
}

fn unsatisfied_content_range_total(headers: &header::HeaderMap) -> Option<u64> {
    let value = headers.get(header::CONTENT_RANGE)?.to_str().ok()?.trim();
    static CONTENT_RANGE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^bytes +\*/(\d+)$").expect("valid unsatisfied Content-Range parser regex")
    });
    CONTENT_RANGE_RE.captures(value)?[1].parse().ok()
}

fn is_html_content_type(headers: &header::HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
}

fn content_type_is_missing(headers: &header::HeaderMap) -> bool {
    headers.get(header::CONTENT_TYPE).is_none()
}

fn content_disposition_filename(headers: &header::HeaderMap) -> Option<String> {
    let value = headers.get(header::CONTENT_DISPOSITION)?;
    let value = String::from_utf8_lossy(value.as_bytes());
    let mut fallback = None;
    for part in split_header_parameters(&value) {
        let Some((name, raw_value)) = part.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("filename*") {
            if let Some(filename) = decode_extended_filename(raw_value) {
                return Some(filename);
            }
        } else if fallback.is_none() && name.trim().eq_ignore_ascii_case("filename") {
            let filename = unquote_header_value(raw_value);
            if !filename.trim().is_empty() {
                fallback = Some(filename);
            }
        }
    }
    fallback
}

fn split_header_parameters(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(value[start..index].trim());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(value[start..].trim());
    parts
}

fn decode_extended_filename(value: &str) -> Option<String> {
    let value = unquote_header_value(value);
    let mut pieces = value.splitn(3, '\'');
    let charset = pieces.next()?;
    let _language = pieces.next()?;
    let encoded = pieces.next()?;
    if !charset.eq_ignore_ascii_case("utf-8") {
        return None;
    }
    percent_decode_utf8(encoded).filter(|filename| !filename.trim().is_empty())
}

fn unquote_header_value(value: &str) -> String {
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_owned();
    };
    let mut output = String::with_capacity(inner.len());
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            output.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            output.push(ch);
        }
    }
    if escaped {
        output.push('\\');
    }
    output
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            let value = u8::from_str_radix(hex, 16).ok()?;
            output.push(value);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn classify_ambiguous_body_probe(bytes: &[u8], key_used: bool) -> Option<GfileError> {
    let prefix_len = bytes.len().min(512);
    let prefix = String::from_utf8_lossy(&bytes[..prefix_len]);
    match classify_page(&prefix, 200) {
        PageState::NeedsKey => {
            return Some(if key_used {
                GfileError::KeyWrong
            } else {
                GfileError::KeyRequired
            });
        }
        PageState::WrongKey => return Some(GfileError::KeyWrong),
        PageState::NotFoundOrExpired => return Some(GfileError::NotFoundOrExpired),
        PageState::Ok | PageState::Unknown => {}
    }

    if looks_like_html(bytes) {
        warn!(
            "download response has no Content-Type and looks like HTML; this may be a false positive for a legitimate HTML file"
        );
        return Some(GfileError::Parse {
            what: "download response body looks like HTML".to_owned(),
            hint: "The server returned an HTML-looking page without Content-Type; rerun with --dump-page and -vv for diagnostics.".to_owned(),
        });
    }
    None
}

fn classify_html_response(body: &str, key_used: bool, fallback_what: &str) -> GfileError {
    match classify_page(body, 200) {
        PageState::NeedsKey => {
            if key_used {
                GfileError::KeyWrong
            } else {
                GfileError::KeyRequired
            }
        }
        PageState::WrongKey => GfileError::KeyWrong,
        PageState::NotFoundOrExpired => GfileError::NotFoundOrExpired,
        PageState::Ok | PageState::Unknown => GfileError::Parse {
            what: fallback_what.to_owned(),
            hint: "The server returned an HTML page instead of a file; rerun with --dump-page and -vv for diagnostics.".to_owned(),
        },
    }
}

fn looks_like_html(bytes: &[u8]) -> bool {
    let prefix_len = bytes.len().min(512);
    let prefix = String::from_utf8_lossy(&bytes[..prefix_len]).to_ascii_lowercase();
    let trimmed = prefix.trim_start();
    trimmed.starts_with("<!doctype html")
        || trimmed.starts_with("<html")
        || trimmed.contains("<html")
        || trimmed.contains("<body")
}

fn warn_on_display_size_mismatch(remote_file: &RemoteFile, expected: Option<u64>) {
    if let (Some(display_size_text), Some(approx), Some(content_length)) = (
        remote_file.display_size.as_deref(),
        remote_file.approx_bytes,
        expected,
    ) {
        let tolerance = (approx / 10).max(1024);
        if approx.abs_diff(content_length) > tolerance {
            warn!(
                display_size = ?display_size_text,
                content_length,
                "display size differs from Content-Length by more than tolerance"
            );
        }
    }
}

fn prompt_or_require_key() -> Result<String, GfileError> {
    if !io::stdin().is_terminal() {
        return Err(GfileError::KeyRequired);
    }
    rpassword::prompt_password("Download key: ")
        .map_err(|source| io_error(source, Path::new("<stdin>"), IoOp::Read))
}

fn redirected_to_gigafile_home(url: &reqwest::Url) -> bool {
    url.host_str() == Some("gigafile.nu") && matches!(url.path(), "" | "/")
}

async fn flush_before_return(writer: &mut BufWriter<File>) {
    let _ = writer.flush().await;
}

fn timeout_network_error(context: &str) -> GfileError {
    GfileError::Network {
        source: boxed(io::Error::new(io::ErrorKind::TimedOut, "stream timed out")),
        context: context.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_label_prefers_resolved_target_name_over_page_name() {
        let target = Path::new("/downloads/実際のファイル名.mmts");

        assert_eq!(
            progress_label(target, "******.mmts"),
            "実際のファイル名.mmts"
        );
    }

    #[test]
    fn progress_label_falls_back_to_page_name_without_file_name() {
        assert_eq!(progress_label(Path::new("/"), "******.mmts"), "******.mmts");
    }

    #[test]
    fn content_disposition_supports_rfc5987_and_quoted_semicolons() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_DISPOSITION,
            header::HeaderValue::from_static(
                "attachment; filename=plain.txt; FILENAME*=UTF-8'en'%E3%83%86%E3%82%B9%E3%83%88.txt",
            ),
        );
        assert_eq!(
            content_disposition_filename(&headers).as_deref(),
            Some("テスト.txt")
        );

        headers.insert(
            header::CONTENT_DISPOSITION,
            header::HeaderValue::from_static("attachment; filename=\"a;b \\\"copy\\\".txt\""),
        );
        assert_eq!(
            content_disposition_filename(&headers).as_deref(),
            Some("a;b \"copy\".txt")
        );
    }

    #[test]
    fn bytes_completed_on_disk_sums_v2_sidecar_not_preallocated_length() {
        let temp = tempfile::TempDir::new().unwrap();
        let part_path = temp.path().join("file.bin.part");
        let sidecar_path = temp.path().join("file.bin.part.json");
        // Preallocated to full size, like a segmented download.
        std::fs::write(&part_path, vec![0_u8; 1000]).unwrap();
        std::fs::write(
            &sidecar_path,
            serde_json::json!({
                "version": 2,
                "file_id": "0123abcd-000000example",
                "expected": 1000,
                "key_used": false,
                "segments": [
                    {"start": 0, "end": 499, "done": true, "downloaded": 500},
                    {"start": 500, "end": 999, "done": false, "downloaded": 120},
                ],
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            bytes_completed_on_disk(&part_path, &sidecar_path),
            Some(620)
        );
    }

    #[test]
    fn bytes_completed_on_disk_uses_part_length_for_v1_sidecar() {
        let temp = tempfile::TempDir::new().unwrap();
        let part_path = temp.path().join("file.bin.part");
        let sidecar_path = temp.path().join("file.bin.part.json");
        std::fs::write(&part_path, vec![0_u8; 321]).unwrap();
        std::fs::write(
            &sidecar_path,
            serde_json::json!({
                "version": 1,
                "file_id": "0123abcd-000000example",
                "expected": 1000,
                "key_used": false,
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            bytes_completed_on_disk(&part_path, &sidecar_path),
            Some(321)
        );
    }

    #[test]
    fn bytes_completed_on_disk_refuses_to_guess_from_corrupt_sidecar() {
        let temp = tempfile::TempDir::new().unwrap();
        let part_path = temp.path().join("file.bin.part");
        let sidecar_path = temp.path().join("file.bin.part.json");
        // Preallocated .part: guessing from its length would claim 100%.
        std::fs::write(&part_path, vec![0_u8; 1000]).unwrap();
        std::fs::write(&sidecar_path, b"{\"version\": 2, \"trunc").unwrap();

        assert_eq!(bytes_completed_on_disk(&part_path, &sidecar_path), None);
    }

    #[test]
    fn bytes_completed_on_disk_rejects_malformed_v2_layout() {
        let temp = tempfile::TempDir::new().unwrap();
        let part_path = temp.path().join("file.bin.part");
        let sidecar_path = temp.path().join("file.bin.part.json");
        std::fs::write(&part_path, vec![0_u8; 10]).unwrap();

        for segments in [
            serde_json::json!([
                {"start": 5, "end": 4, "done": true, "downloaded": 0}
            ]),
            serde_json::json!([
                {"start": 0, "end": 4, "done": true, "downloaded": 5},
                {"start": 6, "end": 9, "done": false, "downloaded": 1}
            ]),
            serde_json::json!([
                {"start": 0, "end": 9, "done": false, "downloaded": 11}
            ]),
        ] {
            std::fs::write(
                &sidecar_path,
                serde_json::json!({
                    "version": 2,
                    "file_id": "0123abcd-000000example",
                    "expected": 10,
                    "key_used": false,
                    "segments": segments,
                })
                .to_string(),
            )
            .unwrap();

            assert_eq!(bytes_completed_on_disk(&part_path, &sidecar_path), None);
        }
    }

    #[test]
    fn bytes_completed_on_disk_rejects_v2_part_length_mismatch() {
        let temp = tempfile::TempDir::new().unwrap();
        let part_path = temp.path().join("file.bin.part");
        let sidecar_path = temp.path().join("file.bin.part.json");
        std::fs::write(&part_path, vec![0_u8; 9]).unwrap();
        std::fs::write(
            &sidecar_path,
            serde_json::json!({
                "version": 2,
                "file_id": "0123abcd-000000example",
                "expected": 10,
                "key_used": false,
                "segments": [
                    {"start": 0, "end": 9, "done": false, "downloaded": 5}
                ],
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(bytes_completed_on_disk(&part_path, &sidecar_path), None);
    }

    #[test]
    fn oversized_sidecar_is_rejected_before_reading_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let sidecar_path = temp.path().join("file.bin.part.json");
        let file = std::fs::File::create(&sidecar_path).unwrap();
        file.set_len(MAX_SIDECAR_BYTES + 1).unwrap();

        assert!(read_sidecar_limited(&sidecar_path).is_err());
    }

    #[test]
    fn bytes_completed_on_disk_uses_part_length_without_sidecar() {
        let temp = tempfile::TempDir::new().unwrap();
        let part_path = temp.path().join("file.bin.part");
        std::fs::write(&part_path, vec![0_u8; 55]).unwrap();

        assert_eq!(
            bytes_completed_on_disk(&part_path, &temp.path().join("file.bin.part.json")),
            Some(55)
        );
    }

    #[test]
    fn segmented_sidecar_layout_must_cover_file_exactly() {
        let mut valid = vec![
            SegmentState {
                start: 0,
                end: 4,
                done: false,
                downloaded: 5,
            },
            SegmentState {
                start: 5,
                end: 9,
                done: false,
                downloaded: 2,
            },
        ];
        assert!(normalize_segments(10, &mut valid));
        assert!(valid[0].done);

        for mut invalid in [
            vec![SegmentState {
                start: 1,
                end: 9,
                done: false,
                downloaded: 0,
            }],
            vec![
                SegmentState {
                    start: 0,
                    end: 4,
                    done: false,
                    downloaded: 0,
                },
                SegmentState {
                    start: 6,
                    end: 9,
                    done: false,
                    downloaded: 0,
                },
            ],
            vec![SegmentState {
                start: 0,
                end: 10,
                done: false,
                downloaded: 0,
            }],
            vec![SegmentState {
                start: 5,
                end: 4,
                done: false,
                downloaded: 0,
            }],
        ] {
            assert!(!normalize_segments(10, &mut invalid));
        }
    }

    #[tokio::test]
    async fn promote_part_never_overwrites_late_target_without_force() {
        let temp = tempfile::TempDir::new().unwrap();
        let part_path = temp.path().join("file.bin.part");
        let sidecar_path = temp.path().join("file.bin.part.json");
        let final_path = temp.path().join("file.bin");
        std::fs::write(&part_path, b"downloaded").unwrap();
        std::fs::write(&sidecar_path, b"sidecar").unwrap();
        std::fs::write(&final_path, b"created while downloading").unwrap();

        let error = promote_part(&part_path, &sidecar_path, &final_path, false)
            .await
            .unwrap_err();

        assert!(matches!(error, GfileError::TargetExists { .. }));
        assert_eq!(
            std::fs::read(&final_path).unwrap(),
            b"created while downloading"
        );
        assert_eq!(std::fs::read(&part_path).unwrap(), b"downloaded");
        assert!(sidecar_path.exists());
    }

    #[tokio::test]
    async fn matomete_force_only_replaces_targets_present_before_batch() {
        let temp = tempfile::TempDir::new().unwrap();
        let part_path = temp.path().join("file.bin.part");
        let sidecar_path = temp.path().join("file.bin.part.json");
        let final_path = temp.path().join("file.bin");
        let targets = BatchTargetRegistry::for_page(PageKind::Matomete, Some(temp.path())).unwrap();
        targets.claim(&final_path, "first-file", true).unwrap();
        targets
            .check_available(&final_path, "first-file", true)
            .unwrap();
        let replace_existing = targets.may_replace_existing(&final_path, true).unwrap();
        assert!(!replace_existing);

        std::fs::write(&part_path, b"downloaded").unwrap();
        std::fs::write(&sidecar_path, b"sidecar").unwrap();
        std::fs::write(&final_path, b"created during batch").unwrap();

        let error = promote_part(&part_path, &sidecar_path, &final_path, replace_existing)
            .await
            .unwrap_err();

        assert!(matches!(error, GfileError::TargetExists { .. }));
        assert_eq!(std::fs::read(&final_path).unwrap(), b"created during batch");
        assert_eq!(std::fs::read(&part_path).unwrap(), b"downloaded");
    }

    #[tokio::test]
    async fn promote_part_reports_success_after_sidecar_cleanup_failure() {
        let temp = tempfile::TempDir::new().unwrap();
        let part_path = temp.path().join("file.bin.part");
        let sidecar_path = temp.path().join("file.bin.part.json");
        let final_path = temp.path().join("file.bin");
        std::fs::write(&part_path, b"downloaded").unwrap();
        std::fs::create_dir(&sidecar_path).unwrap();

        promote_part(&part_path, &sidecar_path, &final_path, false)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&final_path).unwrap(), b"downloaded");
        assert!(!part_path.exists());
        assert!(sidecar_path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn sync_parent_directory_accepts_promoted_target_parent() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("file.bin");
        std::fs::write(&target, b"downloaded").unwrap();

        sync_parent_directory(&target).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn promote_part_refuses_symlink_without_touching_victim() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let victim_path = temp.path().join("victim.bin");
        let part_path = temp.path().join("file.bin.part");
        let sidecar_path = temp.path().join("file.bin.part.json");
        let final_path = temp.path().join("file.bin");
        std::fs::write(&victim_path, b"keep me").unwrap();
        symlink(&victim_path, &part_path).unwrap();
        std::fs::write(&sidecar_path, b"sidecar").unwrap();

        let error = promote_part(&part_path, &sidecar_path, &final_path, false)
            .await
            .unwrap_err();

        assert!(matches!(error, GfileError::Io { .. }));
        assert_eq!(std::fs::read(&victim_path).unwrap(), b"keep me");
        assert!(
            std::fs::symlink_metadata(&part_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!final_path.exists());
        assert!(sidecar_path.exists());
    }
}
