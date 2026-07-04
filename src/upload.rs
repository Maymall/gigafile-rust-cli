// SPDX-License-Identifier: MIT

use std::{
    collections::BTreeMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::TryStreamExt;
use reqwest::{Method, StatusCode, header, multipart};
use serde_json::Value;
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
    task::JoinHandle,
};
use tokio_util::io::ReaderStream;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    error::{BoxError, GfileError, IoOp},
    http,
    parser::{
        download::{PageKind, parse_download_page},
        landing::parse_landing_server,
    },
    progress::{ByteProgress, SegmentProgressSpec, SegmentedProgress},
    timeutil,
    urlinfo::parse_download_url,
};

pub const MIN_CHUNK_SIZE: u64 = 1024 * 1024;
pub const MAX_CHUNK_SIZE: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_CHUNK_SIZE: u64 = 100 * 1024 * 1024;
pub const MIN_UPLOAD_THREADS: u8 = 1;
pub const MAX_UPLOAD_THREADS: u8 = 16;
pub const DEFAULT_UPLOAD_THREADS: u8 = 1;

const DEFAULT_ENTRY_URL: &str = "https://gigafile.nu/";
const UPLOAD_ENDPOINT_PATH: &str = "/upload_chunk.php";
const STREAM_CHUNK_SIZE: usize = 64 * 1024;
const FILE_PART_NAME: &str = "blob";
const FILE_PART_MIME: &str = "application/octet-stream";
const LIFETIME_VALUES: &[u16] = &[3, 5, 7, 14, 30, 60, 100];

// gfile.py@4c45392 lines 108-116 define these multipart fields. Lines 102,
// 105, and 183-184 show chunk numbering starts at 0 and chunk 0 is sent first.
const FIELD_ID: &str = "id";
const FIELD_NAME: &str = "name";
const FIELD_CHUNK: &str = "chunk";
const FIELD_CHUNKS: &str = "chunks";
const FIELD_LIFETIME: &str = "lifetime";
const FIELD_FILE: &str = "file";
const FIRST_CHUNK_INDEX: u64 = 0;

#[derive(Debug, Clone)]
pub struct UploadOptions {
    pub file: PathBuf,
    pub lifetime: u16,
    pub chunk_size: u64,
    pub verify: bool,
    pub timeout: Duration,
    pub retries: u32,
    pub threads: u8,
    pub user_agent: Option<String>,
    pub dump_page: Option<PathBuf>,
    pub quiet: bool,
    pub allow_any_host: bool,
    pub entry_url: String,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            file: PathBuf::new(),
            lifetime: 100,
            chunk_size: DEFAULT_CHUNK_SIZE,
            verify: true,
            timeout: Duration::from_secs(60),
            retries: 3,
            threads: DEFAULT_UPLOAD_THREADS,
            user_agent: None,
            dump_page: None,
            quiet: false,
            allow_any_host: false,
            entry_url: DEFAULT_ENTRY_URL.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadReport {
    pub url: String,
    pub delkey: Option<String>,
    pub remote_filename: Option<String>,
    pub expires_at_estimate: Option<String>,
    pub bytes: u64,
    pub lifetime: u16,
    pub verified: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkPlan {
    index: u64,
    offset: u64,
    len: u64,
}

#[derive(Debug, Clone)]
struct FilePlan {
    path: PathBuf,
    file_name: String,
    size: u64,
    chunks: Vec<ChunkPlan>,
}

#[derive(Debug)]
struct UploadActivity {
    last: Mutex<Instant>,
}

struct ChunkUploadContext<'a> {
    client: &'a reqwest::Client,
    endpoint: &'a str,
    file_plan: &'a FilePlan,
    chunks: u64,
    upload_id: &'a str,
    options: &'a UploadOptions,
    progress: &'a ByteProgress,
}

struct PreparedChunkUploadContext<'a> {
    client: &'a reqwest::Client,
    endpoint: &'a str,
    file_plan: &'a FilePlan,
    chunks: u64,
    upload_id: &'a str,
    options: &'a UploadOptions,
    progress: &'a SegmentedProgress,
}

#[derive(Debug, Clone)]
struct PreparedChunk {
    plan: ChunkPlan,
    body: Arc<Vec<u8>>,
}

#[derive(Debug, Default)]
struct UploadResponseState {
    uploaded_url: Option<String>,
    delkey: Option<String>,
    remote_filename: Option<String>,
}

#[derive(Debug)]
struct UploadCompletion {
    url: String,
    delkey: Option<String>,
    remote_filename: Option<String>,
}

impl UploadActivity {
    fn new() -> Self {
        Self {
            last: Mutex::new(Instant::now()),
        }
    }

    fn mark(&self) {
        *self.last.lock().expect("upload activity mutex poisoned") = Instant::now();
    }

    fn remaining_before_idle_timeout(&self, timeout: Duration) -> Duration {
        let elapsed = self
            .last
            .lock()
            .expect("upload activity mutex poisoned")
            .elapsed();
        timeout.checked_sub(elapsed).unwrap_or(Duration::ZERO)
    }

    fn is_idle_for_at_least(&self, timeout: Duration) -> bool {
        self.last
            .lock()
            .expect("upload activity mutex poisoned")
            .elapsed()
            >= timeout
    }
}

pub fn default_entry_url() -> &'static str {
    DEFAULT_ENTRY_URL
}

pub fn parse_chunk_size(input: &str) -> Result<u64, GfileError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(usage("chunk size must not be empty"));
    }

    let split_at = trimmed
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split_at);
    if number.is_empty() || !number.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(usage(
            "chunk size must be an integer with optional K/M/G suffix",
        ));
    }

    let value = number
        .parse::<u64>()
        .map_err(|_| usage("chunk size is too large"))?;
    let multiplier = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        _ => return Err(usage("chunk size unit must be B, K, M, or G")),
    };
    let bytes = value
        .checked_mul(multiplier)
        .ok_or_else(|| usage("chunk size is too large"))?;
    validate_chunk_size(bytes)?;
    Ok(bytes)
}

pub fn validate_lifetime(lifetime: u16) -> Result<(), GfileError> {
    if LIFETIME_VALUES.contains(&lifetime) {
        Ok(())
    } else {
        Err(usage(
            "lifetime must be one of 3, 5, 7, 14, 30, 60, or 100 days",
        ))
    }
}

pub fn validate_threads(threads: u8) -> Result<u8, GfileError> {
    if (MIN_UPLOAD_THREADS..=MAX_UPLOAD_THREADS).contains(&threads) {
        Ok(threads)
    } else {
        Err(usage(&format!(
            "upload threads must be between {MIN_UPLOAD_THREADS} and {MAX_UPLOAD_THREADS}, got {threads}"
        )))
    }
}

pub async fn upload(options: UploadOptions) -> Result<UploadReport, GfileError> {
    validate_lifetime(options.lifetime)?;
    validate_chunk_size(options.chunk_size)?;
    validate_threads(options.threads)?;
    let file_plan = build_file_plan(&options.file, options.chunk_size).await?;
    let client = http::build_client(options.user_agent.as_deref())?;
    let endpoint = upload_endpoint(&fetch_upload_server(&client, &options).await?);
    let upload_id = Uuid::new_v4().simple().to_string();
    let completion = if options.threads > DEFAULT_UPLOAD_THREADS && file_plan.chunks.len() > 1 {
        upload_chunks_read_ahead(&client, &endpoint, &file_plan, &upload_id, &options).await?
    } else {
        upload_chunks_serial(&client, &endpoint, &file_plan, &upload_id, &options).await?
    };
    let expires_at_estimate = estimate_expires_at(SystemTime::now(), options.lifetime);
    let verified = if options.verify {
        verify_uploaded_file(&client, &completion.url, file_plan.size, &options).await?
    } else {
        None
    };

    Ok(UploadReport {
        url: completion.url,
        delkey: completion.delkey,
        remote_filename: completion.remote_filename,
        expires_at_estimate,
        bytes: file_plan.size,
        lifetime: options.lifetime,
        verified,
    })
}

async fn upload_chunks_serial(
    client: &reqwest::Client,
    endpoint: &str,
    file_plan: &FilePlan,
    upload_id: &str,
    options: &UploadOptions,
) -> Result<UploadCompletion, GfileError> {
    let progress = ByteProgress::new(Some(file_plan.size), options.quiet, &file_plan.file_name);
    let mut state = UploadResponseState::default();
    let mut confirmed_bytes = 0;
    let chunk_context = ChunkUploadContext {
        client,
        endpoint,
        file_plan,
        chunks: file_plan.chunks.len() as u64,
        upload_id,
        options,
        progress: &progress,
    };

    for chunk in &file_plan.chunks {
        let response = send_chunk_with_retries(&chunk_context, *chunk, confirmed_bytes).await?;
        observe_upload_response(*chunk, &response, &mut state);
        confirmed_bytes += chunk.len;
        progress.set_position(confirmed_bytes);
    }
    progress.finish();

    finish_upload_state(state)
}

async fn upload_chunks_read_ahead(
    client: &reqwest::Client,
    endpoint: &str,
    file_plan: &FilePlan,
    upload_id: &str,
    options: &UploadOptions,
) -> Result<UploadCompletion, GfileError> {
    let segments = file_plan
        .chunks
        .iter()
        .map(|chunk| SegmentProgressSpec {
            len: chunk.len,
            initial: 0,
        })
        .collect::<Vec<_>>();
    let progress = SegmentedProgress::new_with_segment_label(
        Some(file_plan.size),
        options.quiet,
        &file_plan.file_name,
        &segments,
        "chunk",
    );
    let chunk_context = PreparedChunkUploadContext {
        client,
        endpoint,
        file_plan,
        chunks: file_plan.chunks.len() as u64,
        upload_id,
        options,
        progress: &progress,
    };
    let mut state = UploadResponseState::default();
    let mut pending = BTreeMap::new();
    let mut ready = BTreeMap::new();
    let mut next_chunk = 0;
    let window = usize::from(options.threads);
    fill_prefetch_window(
        &mut pending,
        ready.len(),
        file_plan,
        &mut next_chunk,
        window,
    );
    if let Err(error) = collect_prefetch_window(&mut pending, &mut ready, &file_plan.path).await {
        abort_prefetches(&mut pending);
        progress.finish();
        return Err(error);
    }

    for chunk in &file_plan.chunks {
        let prepared = match ready.remove(&chunk.index) {
            Some(prepared) => prepared,
            None => {
                let Some(handle) = pending.remove(&chunk.index) else {
                    abort_prefetches(&mut pending);
                    progress.finish();
                    return Err(usage("upload read-ahead queue lost a chunk"));
                };
                match await_prepared_chunk(handle, &file_plan.path).await {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        abort_prefetches(&mut pending);
                        progress.finish();
                        return Err(error);
                    }
                }
            }
        };
        fill_prefetch_window(
            &mut pending,
            ready.len(),
            file_plan,
            &mut next_chunk,
            window,
        );
        let response = match send_prepared_chunk_with_retries(&chunk_context, &prepared).await {
            Ok(response) => response,
            Err(error) => {
                abort_prefetches(&mut pending);
                progress.finish();
                return Err(error);
            }
        };
        observe_upload_response(prepared.plan, &response, &mut state);
        progress.set_segment_position(progress_index(prepared.plan), prepared.plan.len);
    }
    progress.finish();

    finish_upload_state(state)
}

async fn fetch_upload_server(
    client: &reqwest::Client,
    options: &UploadOptions,
) -> Result<String, GfileError> {
    let response = http::get_with_retries_and_timeout(
        client,
        &options.entry_url,
        options.retries,
        "fetching upload page",
        Some(options.timeout),
    )
    .await?;
    let bytes = response
        .bytes()
        .await
        .map_err(|source| network_error(source, "reading upload page body"))?;

    if let Some(path) = &options.dump_page {
        fs::write(path, &bytes)
            .await
            .map_err(|source| io_error(source, path, IoOp::Write))?;
        eprintln!(
            "Warning: dumped page may contain private page details; do not share it publicly."
        );
    }

    let html = String::from_utf8_lossy(&bytes);
    parse_landing_server(&html)
}

fn upload_endpoint(server: &str) -> String {
    let trimmed = server.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        format!("{trimmed}{UPLOAD_ENDPOINT_PATH}")
    } else {
        format!("https://{trimmed}{UPLOAD_ENDPOINT_PATH}")
    }
}

async fn build_file_plan(path: &Path, chunk_size: u64) -> Result<FilePlan, GfileError> {
    let metadata = fs::metadata(path)
        .await
        .map_err(|source| io_error(source, path, IoOp::Metadata))?;
    if !metadata.is_file() {
        return Err(usage("upload path must be a regular file"));
    }
    if metadata.len() == 0 {
        return Err(usage("upload file must not be empty"));
    }
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| usage("upload path must have a file name"))?;

    let chunks = chunk_plans(metadata.len(), chunk_size);
    Ok(FilePlan {
        path: path.to_owned(),
        file_name,
        size: metadata.len(),
        chunks,
    })
}

fn chunk_plans(size: u64, chunk_size: u64) -> Vec<ChunkPlan> {
    let chunk_count = size.div_ceil(chunk_size);
    (FIRST_CHUNK_INDEX..chunk_count)
        .map(|index| {
            let offset = index * chunk_size;
            let remaining = size - offset;
            ChunkPlan {
                index,
                offset,
                len: remaining.min(chunk_size),
            }
        })
        .collect()
}

fn fill_prefetch_window(
    pending: &mut BTreeMap<u64, JoinHandle<Result<PreparedChunk, GfileError>>>,
    ready_len: usize,
    file_plan: &FilePlan,
    next_chunk: &mut usize,
    window: usize,
) {
    while ready_len + pending.len() < window && *next_chunk < file_plan.chunks.len() {
        let chunk = file_plan.chunks[*next_chunk];
        let path = file_plan.path.clone();
        pending.insert(
            chunk.index,
            tokio::spawn(async move { read_prepared_chunk(path, chunk).await }),
        );
        *next_chunk += 1;
    }
}

async fn collect_prefetch_window(
    pending: &mut BTreeMap<u64, JoinHandle<Result<PreparedChunk, GfileError>>>,
    ready: &mut BTreeMap<u64, PreparedChunk>,
    path: &Path,
) -> Result<(), GfileError> {
    let indexes = pending.keys().copied().collect::<Vec<_>>();
    for index in indexes {
        let Some(handle) = pending.remove(&index) else {
            return Err(usage("upload read-ahead queue lost a chunk"));
        };
        let prepared = await_prepared_chunk(handle, path).await?;
        ready.insert(prepared.plan.index, prepared);
    }
    Ok(())
}

fn abort_prefetches(pending: &mut BTreeMap<u64, JoinHandle<Result<PreparedChunk, GfileError>>>) {
    for (_, handle) in std::mem::take(pending) {
        handle.abort();
    }
}

async fn await_prepared_chunk(
    handle: JoinHandle<Result<PreparedChunk, GfileError>>,
    path: &Path,
) -> Result<PreparedChunk, GfileError> {
    handle.await.map_err(|source| {
        io_error(
            io::Error::other(format!("upload read-ahead task failed: {source}")),
            path,
            IoOp::Read,
        )
    })?
}

async fn read_prepared_chunk(path: PathBuf, chunk: ChunkPlan) -> Result<PreparedChunk, GfileError> {
    let mut file = File::open(&path)
        .await
        .map_err(|source| io_error(source, &path, IoOp::Read))?;
    file.seek(SeekFrom::Start(chunk.offset))
        .await
        .map_err(|source| io_error(source, &path, IoOp::Read))?;
    let len = usize::try_from(chunk.len)
        .map_err(|_| usage("upload chunk size is too large for this platform"))?;
    let mut body = vec![0_u8; len];
    file.read_exact(&mut body)
        .await
        .map_err(|source| io_error(source, &path, IoOp::Read))?;

    Ok(PreparedChunk {
        plan: chunk,
        body: Arc::new(body),
    })
}

async fn send_chunk_with_retries(
    context: &ChunkUploadContext<'_>,
    chunk: ChunkPlan,
    confirmed_bytes: u64,
) -> Result<Value, GfileError> {
    let mut attempt = 0;
    loop {
        context.progress.set_position(confirmed_bytes);
        match send_chunk_once(context, chunk, confirmed_bytes).await {
            Ok(value) => return Ok(value),
            Err(error) if upload_retryable(&error) && attempt < context.options.retries => {
                context.progress.set_position(confirmed_bytes);
                warn!(
                    "retrying upload chunk {} after error: {}",
                    chunk.index,
                    error.user_message()
                );
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => {
                context.progress.set_position(confirmed_bytes);
                return Err(error);
            }
        }
    }
}

async fn send_chunk_once(
    context: &ChunkUploadContext<'_>,
    chunk: ChunkPlan,
    confirmed_bytes: u64,
) -> Result<Value, GfileError> {
    let mut file = File::open(&context.file_plan.path)
        .await
        .map_err(|source| io_error(source, &context.file_plan.path, IoOp::Read))?;
    file.seek(SeekFrom::Start(chunk.offset))
        .await
        .map_err(|source| io_error(source, &context.file_plan.path, IoOp::Read))?;
    let reader = file.take(chunk.len);
    let activity = Arc::new(UploadActivity::new());
    let sent_in_attempt = Arc::new(AtomicU64::new(0));
    let activity_for_stream = Arc::clone(&activity);
    let sent_for_stream = Arc::clone(&sent_in_attempt);
    let progress_for_stream = context.progress.clone();
    let stream = ReaderStream::with_capacity(reader, STREAM_CHUNK_SIZE).map_ok(move |bytes| {
        activity_for_stream.mark();
        let sent =
            sent_for_stream.fetch_add(bytes.len() as u64, Ordering::Relaxed) + bytes.len() as u64;
        progress_for_stream.set_position(confirmed_bytes + sent);
        bytes
    });
    let body = reqwest::Body::wrap_stream(stream);
    let part = multipart::Part::stream_with_length(body, chunk.len)
        .file_name(FILE_PART_NAME)
        .mime_str(FILE_PART_MIME)
        .expect("valid multipart MIME type");
    let form = multipart::Form::new()
        .text(FIELD_ID, context.upload_id.to_owned())
        .text(FIELD_NAME, context.file_plan.file_name.clone())
        .text(FIELD_CHUNK, chunk.index.to_string())
        .text(FIELD_CHUNKS, context.chunks.to_string())
        .text(FIELD_LIFETIME, context.options.lifetime.to_string())
        .part(FIELD_FILE, part);

    let request = context.client.post(context.endpoint).multipart(form).send();
    let response = send_with_idle_timeout(
        request,
        activity,
        context.options.timeout,
        "uploading chunk",
    )
    .await?;

    parse_upload_chunk_response(response).await
}

async fn send_prepared_chunk_with_retries(
    context: &PreparedChunkUploadContext<'_>,
    prepared: &PreparedChunk,
) -> Result<Value, GfileError> {
    let mut attempt = 0;
    let progress_index = progress_index(prepared.plan);
    loop {
        context.progress.set_segment_position(progress_index, 0);
        match send_prepared_chunk_once(context, prepared).await {
            Ok(value) => return Ok(value),
            Err(error) if upload_retryable(&error) && attempt < context.options.retries => {
                context.progress.set_segment_position(progress_index, 0);
                warn!(
                    "retrying upload chunk {} after error: {}",
                    prepared.plan.index,
                    error.user_message()
                );
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => {
                context.progress.set_segment_position(progress_index, 0);
                return Err(error);
            }
        }
    }
}

async fn send_prepared_chunk_once(
    context: &PreparedChunkUploadContext<'_>,
    prepared: &PreparedChunk,
) -> Result<Value, GfileError> {
    let activity = Arc::new(UploadActivity::new());
    let sent_in_attempt = Arc::new(AtomicU64::new(0));
    let progress_index = progress_index(prepared.plan);
    let activity_for_stream = Arc::clone(&activity);
    let sent_for_stream = Arc::clone(&sent_in_attempt);
    let progress_for_stream = context.progress.clone();
    let body_for_stream = Arc::clone(&prepared.body);
    let stream = futures_util::stream::unfold((body_for_stream, 0_usize), move |(body, offset)| {
        let activity_for_stream = Arc::clone(&activity_for_stream);
        let sent_for_stream = Arc::clone(&sent_for_stream);
        let progress_for_stream = progress_for_stream.clone();
        async move {
            if offset >= body.len() {
                return None;
            }
            let end = (offset + STREAM_CHUNK_SIZE).min(body.len());
            let bytes = body[offset..end].to_vec();
            activity_for_stream.mark();
            let sent = sent_for_stream.fetch_add(bytes.len() as u64, Ordering::Relaxed)
                + bytes.len() as u64;
            progress_for_stream.set_segment_position(progress_index, sent);
            Some((Ok::<Vec<u8>, io::Error>(bytes), (body, end)))
        }
    });
    let body = reqwest::Body::wrap_stream(stream);
    let part = multipart::Part::stream_with_length(body, prepared.plan.len)
        .file_name(FILE_PART_NAME)
        .mime_str(FILE_PART_MIME)
        .expect("valid multipart MIME type");
    let form = multipart::Form::new()
        .text(FIELD_ID, context.upload_id.to_owned())
        .text(FIELD_NAME, context.file_plan.file_name.clone())
        .text(FIELD_CHUNK, prepared.plan.index.to_string())
        .text(FIELD_CHUNKS, context.chunks.to_string())
        .text(FIELD_LIFETIME, context.options.lifetime.to_string())
        .part(FIELD_FILE, part);

    let request = context.client.post(context.endpoint).multipart(form).send();
    let response = send_with_idle_timeout(
        request,
        activity,
        context.options.timeout,
        "uploading chunk",
    )
    .await?;

    parse_upload_chunk_response(response).await
}

async fn parse_upload_chunk_response(response: reqwest::Response) -> Result<Value, GfileError> {
    if response.status().is_server_error() {
        let status = response.status().as_u16();
        return Err(GfileError::UploadRejected {
            detail: format!(
                "server returned HTTP {} for an upload chunk; re-upload the whole file",
                status
            ),
            status: Some(status),
            retryable: true,
        });
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        return Err(GfileError::UploadRejected {
            detail: format!(
                "server returned HTTP {} for an upload chunk; re-upload the whole file",
                status
            ),
            status: Some(status),
            retryable: false,
        });
    }

    response
        .json::<Value>()
        .await
        .map_err(|source| GfileError::UploadRejected {
            detail: format!(
                "upload endpoint returned a non-JSON response ({source}); re-upload the whole file"
            ),
            status: None,
            retryable: false,
        })
}

async fn send_with_idle_timeout<F>(
    request: F,
    activity: Arc<UploadActivity>,
    timeout: Duration,
    context: &str,
) -> Result<reqwest::Response, GfileError>
where
    F: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    tokio::pin!(request);
    loop {
        let remaining = activity.remaining_before_idle_timeout(timeout);
        tokio::select! {
            result = &mut request => return result.map_err(|source| network_error(source, context)),
            _ = tokio::time::sleep(remaining) => {
                if activity.is_idle_for_at_least(timeout) {
                    return Err(timeout_network_error(context));
                }
            }
        }
    }
}

async fn verify_uploaded_file(
    client: &reqwest::Client,
    uploaded_url: &str,
    expected: u64,
    options: &UploadOptions,
) -> Result<Option<bool>, GfileError> {
    let Ok(url_info) = parse_download_url(uploaded_url, options.allow_any_host) else {
        warn!(
            "upload verification skipped because the returned URL is not a supported download page URL"
        );
        return Ok(None);
    };

    let page = match http::get_with_retries_and_timeout(
        client,
        &url_info.page_url,
        options.retries,
        "fetching uploaded download page",
        Some(options.timeout),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            warn!(
                "upload verification skipped because the download page is unavailable: {}",
                error.user_message()
            );
            return Ok(None);
        }
    };
    let page_bytes = match page.bytes().await {
        Ok(bytes) => bytes,
        Err(source) => {
            warn!(
                "upload verification skipped because the download page body could not be read: {}",
                source
            );
            return Ok(None);
        }
    };
    let html = String::from_utf8_lossy(&page_bytes);
    let page = match parse_download_page(&html, &url_info.file_id) {
        Ok(page) => page,
        Err(error) => {
            warn!(
                "upload verification skipped because the download page could not be parsed: {}",
                error.user_message()
            );
            return Ok(None);
        }
    };
    let file_id = match page.kind {
        PageKind::Single => page.files.first().map(|file| file.file_id.as_str()),
        PageKind::Matomete => page.files.first().map(|file| file.file_id.as_str()),
    };
    let Some(file_id) = file_id else {
        warn!("upload verification skipped because the download page contained no files");
        return Ok(None);
    };

    let download_url = url_info.download_url_for(file_id, None);
    match content_length_via_head(client, &download_url, options).await {
        VerifyProbe::Length(actual) => compare_verified_size(expected, actual).map(Some),
        VerifyProbe::Unsupported => {
            match content_length_via_get(client, &download_url, options).await {
                VerifyProbe::Length(actual) => compare_verified_size(expected, actual).map(Some),
                VerifyProbe::Unsupported | VerifyProbe::Unavailable => {
                    warn!("upload verification skipped because Content-Length is unavailable");
                    Ok(None)
                }
            }
        }
        VerifyProbe::Unavailable => {
            warn!("upload verification skipped because the download endpoint is unavailable");
            Ok(None)
        }
    }
}

enum VerifyProbe {
    Length(u64),
    Unsupported,
    Unavailable,
}

async fn content_length_via_head(
    client: &reqwest::Client,
    url: &str,
    options: &UploadOptions,
) -> VerifyProbe {
    match send_verify_request(client, Method::HEAD, url, options).await {
        Ok(response) if response.status().is_success() => response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(VerifyProbe::Length)
            .unwrap_or(VerifyProbe::Unsupported),
        Ok(response)
            if matches!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
            ) =>
        {
            VerifyProbe::Unsupported
        }
        Ok(_) | Err(_) => VerifyProbe::Unavailable,
    }
}

async fn content_length_via_get(
    client: &reqwest::Client,
    url: &str,
    options: &UploadOptions,
) -> VerifyProbe {
    match send_verify_request(client, Method::GET, url, options).await {
        Ok(response) if response.status().is_success() => response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(VerifyProbe::Length)
            .unwrap_or(VerifyProbe::Unsupported),
        Ok(_) | Err(_) => VerifyProbe::Unavailable,
    }
}

async fn send_verify_request(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    options: &UploadOptions,
) -> Result<reqwest::Response, GfileError> {
    let mut attempt = 0;
    loop {
        let request = client.request(method.clone(), url).send();
        let result = tokio::time::timeout(options.timeout, request)
            .await
            .map_err(|_| timeout_network_error("starting upload verification request"))
            .and_then(|result| {
                result
                    .map_err(|source| network_error(source, "starting upload verification request"))
            });
        match result {
            Ok(response) if response.status().is_server_error() && attempt < options.retries => {
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn compare_verified_size(expected: u64, actual: u64) -> Result<bool, GfileError> {
    if expected == actual {
        Ok(true)
    } else {
        Err(GfileError::VerifyFailed { expected, actual })
    }
}

fn validate_chunk_size(bytes: u64) -> Result<(), GfileError> {
    if (MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&bytes) {
        Ok(())
    } else {
        Err(usage("chunk size must be between 1MiB and 1GiB"))
    }
}

fn upload_retryable(error: &GfileError) -> bool {
    match error {
        GfileError::Network { .. } => true,
        GfileError::UploadRejected { retryable, .. } => *retryable,
        _ => false,
    }
}

fn observe_upload_response(chunk: ChunkPlan, response: &Value, state: &mut UploadResponseState) {
    debug!(
        chunk = chunk.index,
        response = %redact_upload_response(response),
        "upload chunk response"
    );
    if let Some(status) = response.get("status") {
        debug!(?status, chunk = chunk.index, "upload chunk status field");
    }
    if let Some(url) = response
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        state.uploaded_url = Some(url);
    }
    if let Some(delkey) = optional_response_string(response, "delkey") {
        state.delkey = Some(delkey);
    }
    if let Some(filename) = optional_response_string(response, "filename") {
        state.remote_filename = Some(filename);
    }
}

fn finish_upload_state(state: UploadResponseState) -> Result<UploadCompletion, GfileError> {
    let Some(url) = state.uploaded_url else {
        return Err(GfileError::UploadRejected {
            detail:
                "final upload response did not contain a download URL; re-upload the whole file"
                    .to_owned(),
            status: None,
            retryable: false,
        });
    };

    Ok(UploadCompletion {
        url,
        delkey: state.delkey,
        remote_filename: state.remote_filename,
    })
}

fn progress_index(chunk: ChunkPlan) -> usize {
    usize::try_from(chunk.index).expect("upload chunk index fits usize")
}

fn optional_response_string(response: &Value, key: &str) -> Option<String> {
    response
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn redact_upload_response(response: &Value) -> String {
    let mut redacted = response.clone();
    redact_value_key(&mut redacted, "delkey");
    redact_value_key(&mut redacted, "delete_key");
    redacted.to_string()
}

fn redact_value_key(value: &mut Value, key: &str) {
    match value {
        Value::Object(map) => {
            if map.contains_key(key) {
                map.insert(key.to_owned(), Value::String("***".to_owned()));
            }
            for nested in map.values_mut() {
                redact_value_key(nested, key);
            }
        }
        Value::Array(values) => {
            for nested in values {
                redact_value_key(nested, key);
            }
        }
        _ => {}
    }
}

fn estimate_expires_at(now: SystemTime, lifetime_days: u16) -> Option<String> {
    let expires = now.checked_add(Duration::from_secs(u64::from(lifetime_days) * 86_400))?;
    let seconds = expires.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(timeutil::format_unix_utc(seconds))
}

fn usage(message: &str) -> GfileError {
    GfileError::Usage {
        message: message.to_owned(),
    }
}

fn network_error(source: reqwest::Error, context: &str) -> GfileError {
    GfileError::Network {
        source: boxed(source),
        context: context.to_owned(),
    }
}

fn timeout_network_error(context: &str) -> GfileError {
    GfileError::Network {
        source: boxed(io::Error::new(io::ErrorKind::TimedOut, "request timed out")),
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
    fn parse_chunk_size_accepts_binary_suffixes() {
        assert_eq!(parse_chunk_size("1M").unwrap(), MIN_CHUNK_SIZE);
        assert_eq!(parse_chunk_size("50M").unwrap(), 50 * MIN_CHUNK_SIZE);
        assert_eq!(parse_chunk_size("1G").unwrap(), MAX_CHUNK_SIZE);
        assert_eq!(parse_chunk_size("100MiB").unwrap(), DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn parse_chunk_size_rejects_out_of_range_values() {
        assert!(parse_chunk_size("1023K").is_err());
        assert!(parse_chunk_size("2G").is_err());
        assert!(parse_chunk_size("1.5G").is_err());
    }

    #[test]
    fn validate_threads_accepts_upload_range() {
        assert_eq!(validate_threads(1).unwrap(), 1);
        assert_eq!(validate_threads(16).unwrap(), 16);
        assert!(validate_threads(0).is_err());
        assert!(validate_threads(17).is_err());
    }

    #[test]
    fn chunk_plans_are_zero_based_and_cover_file() {
        let chunks = chunk_plans(5, 2);

        assert_eq!(
            chunks,
            vec![
                ChunkPlan {
                    index: 0,
                    offset: 0,
                    len: 2
                },
                ChunkPlan {
                    index: 1,
                    offset: 2,
                    len: 2
                },
                ChunkPlan {
                    index: 2,
                    offset: 4,
                    len: 1
                }
            ]
        );
    }

    #[test]
    fn upload_response_redaction_hides_delkey_fields() {
        let value = serde_json::json!({
            "status": 0,
            "url": "https://23.gigafile.nu/0123abcd-000000example",
            "delkey": "EXAMPLE-DELKEY-0000",
            "nested": { "delete_key": "EXAMPLE-DELETE-0000" }
        });

        let redacted = redact_upload_response(&value);

        assert!(!redacted.contains("EXAMPLE-DELKEY-0000"));
        assert!(!redacted.contains("EXAMPLE-DELETE-0000"));
        assert!(redacted.contains("\"delkey\":\"***\""));
        assert!(redacted.contains("\"delete_key\":\"***\""));
    }
}
