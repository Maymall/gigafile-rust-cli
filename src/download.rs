// SPDX-License-Identifier: GPL-3.0-only

use std::{
    collections::BTreeSet,
    fs::{File as StdFile, OpenOptions as StdOpenOptions},
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use fs2::FileExt;
use regex::Regex;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt, BufWriter, SeekFrom},
};
use tracing::{debug, info, warn};

use crate::{
    error::{BoxError, GfileError, IoOp},
    http,
    jsonout::{self, ErrorJson},
    naming::{log_name_diagnostics, sanitize_server_filename},
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
                indexes.extend(start..=end);
            } else {
                indexes.insert(parse_selection_index(part)?);
            }
        }
        if indexes.is_empty() {
            return Err(selection_usage("selection is empty"));
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

#[derive(Debug, Serialize, Deserialize)]
struct PartSidecar {
    version: u8,
    file_id: String,
    expected: Option<u64>,
    key_used: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentSidecar {
    version: u8,
    file_id: String,
    expected: u64,
    key_used: bool,
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
}

#[derive(Debug, Clone, Copy)]
struct TransferPlan {
    append: bool,
    initial_bytes: u64,
    expected_total: Option<u64>,
    resumed: bool,
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
    timeout: Duration,
    progress: SegmentedProgress,
    shared_segments: Arc<Mutex<Vec<SegmentState>>>,
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
            warn!(
                "failed to release download lock {}: {source}",
                self.path.display()
            );
        }
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
    let client = http::build_client(options.user_agent.as_deref())?;

    let page_response = http::get_with_retries(
        &client,
        &url_info.page_url,
        options.retries,
        "fetching page",
    )
    .await?;
    let page_status = page_response.status().as_u16();
    let final_page_url = page_response.url().clone();
    let page_bytes = page_response
        .bytes()
        .await
        .map_err(|source| network_error(source, "reading download page body"))?;

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

    let selected_files = selected_files(&page, options.selection.as_ref());
    let mut records = Vec::with_capacity(selected_files.len());
    let mut first_error = None;
    for remote_file in selected_files {
        let final_path =
            resolve_output_path(remote_file, page.kind, options.output.as_deref()).await?;
        if let Err(error) = ensure_target_available(&final_path, options.force) {
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

        match download_file_with_retries(&client, &download_url, remote_file, &final_path, &options)
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
) -> Result<SingleDownloadOutcome, GfileError> {
    let _download_lock = DownloadLock::acquire(final_path)?;
    let mut attempt = 0;
    loop {
        match try_download_file(client, download_url, remote_file, final_path, options).await {
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
) -> Result<SingleDownloadOutcome, GfileError> {
    if options.threads > DEFAULT_DOWNLOAD_THREADS {
        return try_download_file_segmented_or_fallback(
            client,
            download_url,
            remote_file,
            final_path,
            options,
        )
        .await;
    }

    try_download_file_sequential(client, download_url, remote_file, final_path, options).await
}

async fn try_download_file_sequential(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
) -> Result<SingleDownloadOutcome, GfileError> {
    let target_path = final_path.to_owned();
    let header_output_dir = header_filename_output_dir(final_path, options.output.as_deref())?;
    let mut resume = prepare_resume(&target_path, remote_file, options).await?;
    let mut response =
        send_download_request(client, download_url, resume.range_start, options).await?;

    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
        if let Some(outcome) =
            complete_if_range_already_finished(&resume, &target_path, options).await?
        {
            return Ok(outcome);
        }
        warn!("server rejected resume range before expected size; restarting from zero");
        remove_if_exists(&resume.part_path).await?;
        remove_if_exists(&resume.sidecar_path).await?;
        resume.range_start = None;
        resume.expected = None;
        response = send_download_request(client, download_url, None, options).await?;
    }

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
    )
    .await
}

async fn consume_download_response_sequential(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    plan: SequentialDownloadPlan,
    options: &DownloadOptions,
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
        let body = response
            .text()
            .await
            .map_err(|source| network_error(source, "reading HTML download error body"))?;
        return Err(classify_html_response(
            &body,
            options.key.is_some(),
            "download response content-type is HTML",
        ));
    }

    let header_name = content_disposition_filename(response.headers());
    let _header_target_lock = if resume.range_start.is_none() {
        if let (Some(dir), Some(name)) = (header_output_dir.as_deref(), header_name.as_deref()) {
            let header_path = dir.join(sanitize_server_filename(name, &remote_file.file_id));
            if header_path != target_path {
                ensure_target_available(&header_path, options.force)?;
                let lock = DownloadLock::acquire(&header_path)?;
                let header_resume = prepare_resume(&header_path, remote_file, options).await?;
                let should_retry_with_header_resume = header_resume.range_start.is_some();
                target_path = header_path;
                resume = header_resume;
                if should_retry_with_header_resume {
                    response =
                        send_download_request(client, download_url, resume.range_start, options)
                            .await?;
                    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                        if let Some(outcome) =
                            complete_if_range_already_finished(&resume, &target_path, options)
                                .await?
                        {
                            return Ok(outcome);
                        }
                        warn!(
                            "server rejected resume range before expected size; restarting from zero"
                        );
                        remove_if_exists(&resume.part_path).await?;
                        remove_if_exists(&resume.sidecar_path).await?;
                        resume.range_start = None;
                        resume.expected = None;
                        response =
                            send_download_request(client, download_url, None, options).await?;
                    }
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

    if !response.status().is_success() {
        return Err(http::status_error(response.status(), download_url));
    }

    if is_html_content_type(response.headers()) {
        let body = response
            .text()
            .await
            .map_err(|source| network_error(source, "reading HTML download error body"))?;
        return Err(classify_html_response(
            &body,
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

    if content_type_is_missing(response.headers()) {
        if let Some(chunk) = first_chunk.as_deref() {
            if let Some(error) = classify_ambiguous_body_probe(chunk, options.key.is_some()) {
                return Err(error);
            }
        }
    }

    write_sidecar(
        &resume.sidecar_path,
        remote_file,
        transfer.expected_total,
        options.key.is_some(),
    )
    .await?;

    let file = if transfer.append {
        OpenOptions::new()
            .append(true)
            .open(&resume.part_path)
            .await
            .map_err(|source| io_error(source, &resume.part_path, IoOp::Write))?
    } else {
        File::create(&resume.part_path)
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

    if let Some(expected) = transfer.expected_total {
        if actual != expected {
            writer
                .flush()
                .await
                .map_err(|source| io_error(source, &resume.part_path, IoOp::Write))?;
            return Err(GfileError::SizeMismatch { expected, actual });
        }
    }

    writer
        .flush()
        .await
        .map_err(|source| io_error(source, &resume.part_path, IoOp::Write))?;
    let file = writer.into_inner();
    file.sync_all()
        .await
        .map_err(|source| io_error(source, &resume.part_path, IoOp::Write))?;

    promote_part(
        &resume.part_path,
        &resume.sidecar_path,
        &target_path,
        options.force,
    )
    .await?;

    info!("downloaded {} bytes to {}", actual, target_path.display());

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
        )
        .await;
    }

    try_download_file_segmented_fresh(client, download_url, remote_file, final_path, options).await
}

async fn try_download_file_segmented_fresh(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
) -> Result<SingleDownloadOutcome, GfileError> {
    let range_end = initial_segment_end(remote_file, options.threads);
    let response = send_range_request(client, download_url, 0, range_end, options.timeout).await?;
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
        return sequential_fallback(client, download_url, remote_file, final_path, options).await;
    }

    if is_html_content_type(response.headers()) {
        let body = response
            .text()
            .await
            .map_err(|source| network_error(source, "reading HTML download error body"))?;
        return Err(classify_html_response(
            &body,
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
        return sequential_fallback(client, download_url, remote_file, final_path, options).await;
    }
    let Some(expected) = content_range.total else {
        warn!(
            "segmented download response has no Content-Range total; falling back to one connection"
        );
        return sequential_fallback(client, download_url, remote_file, final_path, options).await;
    };
    if expected == 0 {
        warn!("empty file download uses the single-connection path");
        return sequential_fallback(client, download_url, remote_file, final_path, options).await;
    }
    warn_on_display_size_mismatch(remote_file, Some(expected));

    let header_name = content_disposition_filename(response.headers());
    let header_output_dir = header_filename_output_dir(final_path, options.output.as_deref())?;
    let mut target_path = final_path.to_owned();
    let _header_target_lock =
        if let (Some(dir), Some(name)) = (header_output_dir.as_deref(), header_name.as_deref()) {
            let header_path = dir.join(sanitize_server_filename(name, &remote_file.file_id));
            if header_path != target_path {
                ensure_target_available(&header_path, options.force)?;
                let lock = DownloadLock::acquire(&header_path)?;
                target_path = header_path;
                Some(lock)
            } else {
                None
            }
        } else {
            None
        };

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
        )
        .await;
    }

    let (part_path, sidecar_path) = part_paths(&target_path)?;
    let segments = build_segments_from_initial(expected, options.threads, content_range.end);
    let segment_resume = SegmentResumePlan {
        part_path,
        sidecar_path,
        expected,
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
    )
    .await
    {
        Ok(outcome) => Ok(outcome),
        Err(SegmentDownloadError::Fallback(reason)) => {
            warn!(
                "segmented download was not accepted by the server: {reason}; falling back to one connection"
            );
            remove_if_exists(&part_path).await?;
            remove_if_exists(&sidecar_path).await?;
            sequential_fallback(client, download_url, remote_file, &target_path, options).await
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
) -> Result<SingleDownloadOutcome, GfileError> {
    let Some((index, segment)) = first_incomplete_segment(&segment_resume.segments) else {
        promote_part(
            &segment_resume.part_path,
            &segment_resume.sidecar_path,
            final_path,
            options.force,
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
        return sequential_fallback(client, download_url, remote_file, final_path, options).await;
    }

    let content_range = parse_content_range(response.headers())?;
    if content_range.start != range_start || content_range.end != segment.end {
        warn!(
            "segmented download was not accepted by the server: Content-Range was {}-{}, expected {range_start}-{}; falling back to one connection",
            content_range.start, content_range.end, segment.end
        );
        remove_if_exists(&segment_resume.part_path).await?;
        remove_if_exists(&segment_resume.sidecar_path).await?;
        return sequential_fallback(client, download_url, remote_file, final_path, options).await;
    }
    if let Some(total) = content_range.total {
        if total != segment_resume.expected {
            warn!(
                "segmented download was not accepted by the server: Content-Range total is {total}, expected {}; falling back to one connection",
                segment_resume.expected
            );
            remove_if_exists(&segment_resume.part_path).await?;
            remove_if_exists(&segment_resume.sidecar_path).await?;
            return sequential_fallback(client, download_url, remote_file, final_path, options)
                .await;
        }
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
    )
    .await
    {
        Ok(outcome) => Ok(outcome),
        Err(SegmentDownloadError::Fallback(reason)) => {
            warn!(
                "segmented download was not accepted by the server: {reason}; falling back to one connection"
            );
            remove_if_exists(&part_path).await?;
            remove_if_exists(&sidecar_path).await?;
            sequential_fallback(client, download_url, remote_file, final_path, options).await
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
    )
    .await
}

async fn sequential_fallback(
    client: &reqwest::Client,
    download_url: &str,
    remote_file: &RemoteFile,
    final_path: &Path,
    options: &DownloadOptions,
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
) -> Result<SingleDownloadOutcome, SegmentDownloadError> {
    let SegmentedDownloadPlan {
        header_name,
        resume: segment_resume,
        initial,
    } = plan;
    let expected = segment_resume.expected;
    let part_file = if segment_resume.resumed {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&segment_resume.part_path)
            .await
            .map_err(|source| {
                SegmentDownloadError::Failed(io_error(
                    source,
                    &segment_resume.part_path,
                    IoOp::Write,
                ))
            })?
    } else {
        File::create(&segment_resume.part_path)
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
        &segment_resume.segments,
    )
    .await
    .map_err(SegmentDownloadError::Failed)?;

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
        timeout: options.timeout,
        progress: progress.clone(),
        shared_segments: Arc::clone(&shared_segments),
    };
    let mut handles = Vec::new();
    let initial_index = initial.index;
    let initial_context = context.clone();
    handles.push(tokio::spawn(async move {
        consume_segment_response(
            &initial_context,
            initial.index,
            initial.range_start,
            initial.response,
        )
        .await
    }));
    for (index, segment) in segment_resume.segments.iter().enumerate() {
        if index == initial_index || segment.done {
            continue;
        }
        let context = context.clone();
        let retries = options.retries;
        handles.push(tokio::spawn(async move {
            download_segment_with_retries(context, index, retries).await
        }));
    }

    let mut first_worker_error = None;
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) if first_worker_error.is_none() => {
                first_worker_error = Some(error);
            }
            Ok(Err(_)) => {}
            Err(source) => {
                if first_worker_error.is_none() {
                    first_worker_error = Some(SegmentDownloadError::Failed(GfileError::Network {
                        source: boxed(source),
                        context: "joining segmented download worker".to_owned(),
                    }));
                }
            }
        }
    }
    if let Some(error) = first_worker_error {
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
        &final_segments,
    )
    .await
    .map_err(SegmentDownloadError::Failed)?;

    let file = OpenOptions::new()
        .write(true)
        .open(&segment_resume.part_path)
        .await
        .map_err(|source| {
            SegmentDownloadError::Failed(io_error(source, &segment_resume.part_path, IoOp::Write))
        })?;
    file.sync_all().await.map_err(|source| {
        SegmentDownloadError::Failed(io_error(source, &segment_resume.part_path, IoOp::Write))
    })?;

    promote_part(
        &segment_resume.part_path,
        &segment_resume.sidecar_path,
        final_path,
        options.force,
    )
    .await
    .map_err(SegmentDownloadError::Failed)?;

    info!("downloaded {} bytes to {}", expected, final_path.display());

    Ok(SingleDownloadOutcome {
        name: header_name,
        path: final_path.to_owned(),
        bytes: expected,
        resumed: segment_resume.resumed,
        threads: final_segments.len() as u8,
    })
}

async fn download_segment_with_retries(
    context: SegmentContext,
    index: usize,
    retries: u32,
) -> Result<(), SegmentDownloadError> {
    let mut attempt = 0;
    loop {
        match try_download_segment(&context, index).await {
            Ok(()) => return Ok(()),
            Err(SegmentDownloadError::Fallback(reason)) => {
                return Err(SegmentDownloadError::Fallback(reason));
            }
            Err(SegmentDownloadError::Failed(error))
                if http::is_retryable(&error) && attempt < retries =>
            {
                warn!(
                    "retrying download segment {} after error: {}",
                    index + 1,
                    error.user_message()
                );
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn try_download_segment(
    context: &SegmentContext,
    index: usize,
) -> Result<(), SegmentDownloadError> {
    let segment =
        segment_at(&context.shared_segments, index).map_err(SegmentDownloadError::Failed)?;
    let segment_len = segment_len(&segment);
    let already_downloaded = segment.downloaded.min(segment_len);
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
    if let Some(total) = content_range.total {
        if total != context.expected {
            return Err(SegmentDownloadError::Fallback(format!(
                "Content-Range total is {total}, expected {}",
                context.expected
            )));
        }
    }

    let mut file = OpenOptions::new()
        .write(true)
        .open(&context.part_path)
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
        update_segment_sidecar_sync(context, index, downloaded, false)
            .map_err(SegmentDownloadError::Failed)?;
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
    timeout: Duration,
) -> Result<reqwest::Response, GfileError> {
    let request = client
        .get(download_url)
        .header(header::RANGE, format!("bytes={start}-{end}"));
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
        debug!(
            path = %part_path.display(),
            "looked for existing segmented .part, not found"
        );
        return Ok(None);
    }

    let sidecar = match fs::read(&sidecar_path).await {
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
        || !normalize_segments(&mut sidecar.segments)
    {
        warn!(
            "existing .part sidecar cannot be used for this segmented download; restarting from zero. {THREADS_RESUME_HINT}"
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

fn normalize_segments(segments: &mut [SegmentState]) -> bool {
    for segment in segments {
        let len = segment_len(segment);
        if segment.downloaded > len {
            return false;
        }
        if segment.done {
            segment.downloaded = len;
        } else if segment.downloaded == len {
            segment.done = true;
        }
    }
    true
}

fn segment_at(
    segments: &Arc<Mutex<Vec<SegmentState>>>,
    index: usize,
) -> Result<SegmentState, GfileError> {
    let guard = segments.lock().map_err(|_| GfileError::Parse {
        what: "segmented download state lock was poisoned".to_owned(),
        hint: "This is an internal state error; please report it.".to_owned(),
    })?;
    guard.get(index).cloned().ok_or_else(|| GfileError::Parse {
        what: format!("missing segment state at index {index}"),
        hint: "This is an internal state error; please report it.".to_owned(),
    })
}

fn segment_snapshot(
    segments: &Arc<Mutex<Vec<SegmentState>>>,
) -> Result<Vec<SegmentState>, GfileError> {
    segments
        .lock()
        .map(|segments| segments.clone())
        .map_err(|_| GfileError::Parse {
            what: "segmented download state lock was poisoned".to_owned(),
            hint: "This is an internal state error; please report it.".to_owned(),
        })
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
        .map_err(|_| GfileError::Parse {
            what: "segmented download state lock was poisoned".to_owned(),
            hint: "This is an internal state error; please report it.".to_owned(),
        })?;
    let Some(segment) = segments.get_mut(index) else {
        return Err(GfileError::Parse {
            what: format!("missing segment state at index {index}"),
            hint: "This is an internal state error; please report it.".to_owned(),
        });
    };
    segment.downloaded = downloaded;
    segment.done = done;
    let bytes = segment_sidecar_bytes(
        &context.file_id,
        context.expected,
        context.key_used,
        &segments,
    )?;
    std::fs::write(&context.sidecar_path, bytes)
        .map_err(|source| io_error(source, &context.sidecar_path, IoOp::Write))
}

async fn write_segment_sidecar(
    sidecar_path: &Path,
    file_id: &str,
    expected: u64,
    key_used: bool,
    segments: &[SegmentState],
) -> Result<(), GfileError> {
    let sidecar_bytes = segment_sidecar_bytes(file_id, expected, key_used, segments)?;
    fs::write(sidecar_path, sidecar_bytes)
        .await
        .map_err(|source| io_error(source, sidecar_path, IoOp::Write))
}

fn segment_sidecar_bytes(
    file_id: &str,
    expected: u64,
    key_used: bool,
    segments: &[SegmentState],
) -> Result<Vec<u8>, GfileError> {
    let sidecar = SegmentSidecar {
        version: 2,
        file_id: file_id.to_owned(),
        expected,
        key_used,
        segments: segments.to_vec(),
    };
    serde_json::to_vec(&sidecar).map_err(|source| GfileError::Parse {
        what: format!("failed to serialize segmented sidecar: {source}"),
        hint: "This is an internal state error; please report it.".to_owned(),
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

async fn send_download_request(
    client: &reqwest::Client,
    download_url: &str,
    range_start: Option<u64>,
    options: &DownloadOptions,
) -> Result<reqwest::Response, GfileError> {
    let mut request = client.get(download_url);
    if let Some(start) = range_start {
        request = request.header(header::RANGE, format!("bytes={start}-"));
    }
    let result = tokio::time::timeout(options.timeout, request.send())
        .await
        .map_err(|_| timeout_network_error("starting file download"))?;
    result.map_err(|source| network_error(source, "starting file download"))
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
        });
    }

    let sidecar = match fs::read(&sidecar_path).await {
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
        });
    };

    if sidecar.version != 1 || sidecar.file_id != remote_file.file_id || sidecar.expected.is_none()
    {
        warn!(
            "existing .part sidecar does not match this file; restarting from zero. {THREADS_RESUME_HINT}"
        );
        return Ok(ResumePlan {
            part_path,
            sidecar_path,
            range_start: None,
            expected: None,
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
        });
    }

    Ok(ResumePlan {
        part_path,
        sidecar_path,
        range_start: Some(len),
        expected: sidecar.expected,
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
    resume: &ResumePlan,
    final_path: &Path,
    options: &DownloadOptions,
) -> Result<Option<SingleDownloadOutcome>, GfileError> {
    let Some(start) = resume.range_start else {
        return Ok(None);
    };
    if resume.expected != Some(start) {
        return Ok(None);
    }
    promote_part(
        &resume.part_path,
        &resume.sidecar_path,
        final_path,
        options.force,
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

async fn write_sidecar(
    sidecar_path: &Path,
    remote_file: &RemoteFile,
    expected: Option<u64>,
    key_used: bool,
) -> Result<(), GfileError> {
    let sidecar = PartSidecar {
        version: 1,
        file_id: remote_file.file_id.clone(),
        expected,
        key_used,
    };
    let sidecar_bytes = serde_json::to_vec(&sidecar).map_err(|source| GfileError::Parse {
        what: format!("failed to serialize sidecar: {source}"),
        hint: "This is an internal state error; please report it.".to_owned(),
    })?;
    fs::write(sidecar_path, sidecar_bytes)
        .await
        .map_err(|source| io_error(source, sidecar_path, IoOp::Write))
}

async fn promote_part(
    part_path: &Path,
    sidecar_path: &Path,
    final_path: &Path,
    force: bool,
) -> Result<(), GfileError> {
    if final_path.exists() && force {
        fs::remove_file(final_path)
            .await
            .map_err(|source| io_error(source, final_path, IoOp::Rename))?;
    }

    remove_if_exists(sidecar_path).await?;
    fs::rename(part_path, final_path)
        .await
        .map_err(|source| io_error(source, final_path, IoOp::Rename))
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
) -> Result<Option<Vec<u8>>, ChunkReadError> {
    match tokio::time::timeout(timeout, response.chunk()).await {
        Ok(Ok(Some(chunk))) => Ok(Some(chunk.to_vec())),
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
    if let Some(path) = output {
        if !(path.exists() && path.is_dir()) {
            return Err(GfileError::Usage {
                message: format!(
                    "matomete downloads require --output to be an existing directory, got {}",
                    path.display()
                ),
            });
        }
    }
    Ok(())
}

fn ensure_target_available(final_path: &Path, force: bool) -> Result<(), GfileError> {
    if final_path.exists() && !force {
        return Err(io_error(
            io::Error::new(io::ErrorKind::AlreadyExists, "target exists"),
            final_path,
            IoOp::Create,
        ));
    }
    Ok(())
}

fn part_paths(final_path: &Path) -> Result<(PathBuf, PathBuf), GfileError> {
    let file_name = final_path.file_name().ok_or_else(|| {
        io_error(
            io::Error::new(io::ErrorKind::InvalidInput, "target path has no filename"),
            final_path,
            IoOp::Create,
        )
    })?;
    let part_name = format!("{}.part", file_name.to_string_lossy());
    let sidecar_name = format!("{part_name}.json");

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
    lock_path.set_file_name(format!("{}.lock", file_name.to_string_lossy()));
    Ok(lock_path)
}

fn is_lock_contention(source: &io::Error) -> bool {
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
    })
}

// The page display name can be a server-side mask (e.g. `******.ext`); the
// resolved target path already carries the Content-Disposition name, so the
// progress bar must label with the target, not the page name.
fn progress_label(target_path: &Path, fallback: &str) -> String {
    target_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| fallback.to_owned())
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
    let re =
        Regex::new(r"^bytes +(\d+)-(\d+)/(\d+|\*)$").expect("valid Content-Range parser regex");
    let captures = re.captures(value).ok_or_else(|| GfileError::Parse {
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
    Ok(ContentRange { start, end, total })
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
    for part in value.split(';').map(str::trim) {
        if let Some(encoded) = part.strip_prefix("filename*=") {
            let encoded = encoded.trim_matches('"');
            let encoded = encoded
                .strip_prefix("UTF-8''")
                .or_else(|| encoded.strip_prefix("utf-8''"))
                .unwrap_or(encoded);
            if let Some(decoded) = percent_decode_utf8(encoded) {
                return Some(decoded);
            }
        }
    }
    for part in value.split(';').map(str::trim) {
        if let Some(filename) = part.strip_prefix("filename=") {
            let filename = filename.trim_matches('"').trim();
            if !filename.is_empty() {
                return Some(filename.to_owned());
            }
        }
    }
    None
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
                "display size {} differs from Content-Length {} by more than tolerance",
                display_size_text, content_length
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

fn network_error(source: reqwest::Error, context: &str) -> GfileError {
    GfileError::Network {
        source: boxed(source),
        context: context.to_owned(),
    }
}

fn timeout_network_error(context: &str) -> GfileError {
    GfileError::Network {
        source: boxed(io::Error::new(io::ErrorKind::TimedOut, "stream timed out")),
        context: context.to_owned(),
    }
}

fn io_error(source: io::Error, path: &Path, op: IoOp) -> GfileError {
    GfileError::Io {
        source,
        path: path.to_owned(),
        op,
    }
}

fn boxed(error: impl std::error::Error + Send + Sync + 'static) -> BoxError {
    Box::new(error)
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
}
