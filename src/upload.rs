// SPDX-License-Identifier: MIT

use std::{
    collections::BTreeMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures_util::TryStreamExt;
use reqwest::{StatusCode, header, multipart};
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
    error::{GfileError, IoOp, boxed, io_error, network_error, usage},
    http,
    naming::escape_terminal_text,
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
const MAX_READ_AHEAD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_UPLOAD_CHUNKS: u64 = 100_000;
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
    source: Arc<std::fs::File>,
    fingerprint: SourceFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceFingerprint {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug)]
struct ChunkAttemptError {
    error: GfileError,
    body_started: bool,
}

#[derive(Debug)]
struct UploadActivity {
    started: Instant,
    last_activity_ns: AtomicU64,
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
    body: Bytes,
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
            started: Instant::now(),
            last_activity_ns: AtomicU64::new(0),
        }
    }

    fn mark(&self) {
        self.last_activity_ns
            .store(self.elapsed_ns(), Ordering::Relaxed);
    }

    fn remaining_before_idle_timeout(&self, timeout: Duration) -> Duration {
        let elapsed = Duration::from_nanos(
            self.elapsed_ns()
                .saturating_sub(self.last_activity_ns.load(Ordering::Relaxed)),
        );
        timeout.checked_sub(elapsed).unwrap_or(Duration::ZERO)
    }

    fn is_idle_for_at_least(&self, timeout: Duration) -> bool {
        self.elapsed_ns()
            .saturating_sub(self.last_activity_ns.load(Ordering::Relaxed))
            >= duration_ns(timeout)
    }

    fn elapsed_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
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
        Err(usage(format!(
            "upload threads must be between {MIN_UPLOAD_THREADS} and {MAX_UPLOAD_THREADS}, got {threads}"
        )))
    }
}

pub async fn upload(options: UploadOptions) -> Result<UploadReport, GfileError> {
    validate_lifetime(options.lifetime)?;
    validate_chunk_size(options.chunk_size)?;
    validate_threads(options.threads)?;
    let file_plan = build_file_plan(&options.file, options.chunk_size).await?;
    let client =
        http::build_gigafile_client(options.user_agent.as_deref(), options.allow_any_host)?;
    let endpoint = upload_endpoint(
        &fetch_upload_server(&client, &options).await?,
        options.allow_any_host,
    )?;
    let upload_id = Uuid::new_v4().simple().to_string();
    let read_ahead_window = bounded_read_ahead_window(
        options.threads,
        file_plan
            .chunks
            .iter()
            .map(|chunk| chunk.len)
            .max()
            .unwrap_or(1),
    );
    let completion = if read_ahead_window > 1 && file_plan.chunks.len() > 1 {
        upload_chunks_read_ahead(
            &client,
            &endpoint,
            &file_plan,
            &upload_id,
            &options,
            read_ahead_window,
        )
        .await?
    } else {
        if options.threads > DEFAULT_UPLOAD_THREADS && file_plan.chunks.len() > 1 {
            warn!(
                requested = options.threads,
                budget_bytes = MAX_READ_AHEAD_BYTES,
                "upload chunks are too large for bounded read-ahead; using streaming upload"
            );
        }
        upload_chunks_serial(&client, &endpoint, &file_plan, &upload_id, &options).await?
    };
    if parse_download_url(&completion.url, options.allow_any_host).is_err() {
        return Err(GfileError::UploadRejected {
            detail: "upload response contained an unsupported download URL".to_owned(),
            status: None,
            retryable: false,
        });
    }
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
    let display_name = escape_terminal_text(&file_plan.file_name);
    let progress = ByteProgress::new(Some(file_plan.size), options.quiet, &display_name);
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
        if let Err(error) = validate_source_unchanged(file_plan).await {
            progress.finish();
            return Err(error);
        }
        let response = match send_chunk_with_retries(&chunk_context, *chunk, confirmed_bytes).await
        {
            Ok(response) => response,
            Err(error) => {
                progress.finish();
                return Err(error);
            }
        };
        if let Err(error) = observe_upload_response(*chunk, &response, &mut state) {
            progress.finish();
            return Err(error);
        }
        confirmed_bytes += chunk.len;
        progress.set_position(confirmed_bytes);
    }
    if let Err(error) = validate_source_unchanged(file_plan).await {
        progress.finish();
        return Err(error);
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
    window: usize,
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
        &escape_terminal_text(&file_plan.file_name),
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
    let mut next_chunk = 0;
    if window < usize::from(options.threads) {
        warn!(
            requested = options.threads,
            effective = window,
            budget_bytes = MAX_READ_AHEAD_BYTES,
            "reducing upload read-ahead window to stay within the memory budget"
        );
    }
    fill_prefetch_window(
        &mut pending,
        // Read only chunk zero before the first POST. The rest of the window
        // is launched once that chunk is ready and overlaps its upload.
        window.saturating_sub(1),
        file_plan,
        &mut next_chunk,
        window,
    );

    for chunk in &file_plan.chunks {
        let Some(handle) = pending.remove(&chunk.index) else {
            abort_prefetches(&mut pending);
            progress.finish();
            return Err(usage("upload read-ahead queue lost a chunk"));
        };
        let prepared = match await_prepared_chunk(handle, &file_plan.path).await {
            Ok(prepared) => prepared,
            Err(error) => {
                abort_prefetches(&mut pending);
                progress.finish();
                return Err(error);
            }
        };
        if let Err(error) = validate_source_unchanged(file_plan).await {
            abort_prefetches(&mut pending);
            progress.finish();
            return Err(error);
        }
        fill_prefetch_window(
            &mut pending,
            // `prepared` remains resident while its request is in flight, so
            // count it as one slot in the total memory budget.
            1,
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
        if let Err(error) = observe_upload_response(prepared.plan, &response, &mut state) {
            abort_prefetches(&mut pending);
            progress.finish();
            return Err(error);
        }
        progress.set_segment_position(progress_index(prepared.plan), prepared.plan.len);
    }
    if let Err(error) = validate_source_unchanged(file_plan).await {
        abort_prefetches(&mut pending);
        progress.finish();
        return Err(error);
    }
    progress.finish();

    finish_upload_state(state)
}

fn bounded_read_ahead_window(requested: u8, largest_chunk: u64) -> usize {
    let budget_window =
        usize::try_from(MAX_READ_AHEAD_BYTES / largest_chunk.max(1)).unwrap_or(usize::MAX);
    usize::from(requested).min(budget_window)
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
    let bytes = http::read_body_limited(
        response,
        http::PAGE_BODY_LIMIT,
        options.timeout,
        "reading upload page body",
    )
    .await?;

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

fn upload_endpoint(server: &str, allow_any_host: bool) -> Result<String, GfileError> {
    let trimmed = server.trim().trim_end_matches('/');
    let candidate = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let mut url = reqwest::Url::parse(&candidate).map_err(|_| GfileError::Parse {
        what: "upload page contained an invalid server URL".to_owned(),
        hint: "The upload page structure may have changed; rerun with --dump-page and -vv."
            .to_owned(),
    })?;
    let host = url.host_str().unwrap_or_default();
    let valid_host = if allow_any_host {
        matches!(url.scheme(), "http" | "https") && !host.is_empty()
    } else {
        url.scheme() == "https" && url.port().is_none() && numeric_gigafile_host(host)
    };
    if !valid_host
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(GfileError::Parse {
            what: "upload page pointed to an untrusted server".to_owned(),
            hint: "The upload page structure may have changed; rerun with --dump-page and -vv."
                .to_owned(),
        });
    }
    url.set_path(UPLOAD_ENDPOINT_PATH);
    Ok(url.to_string())
}

fn numeric_gigafile_host(host: &str) -> bool {
    host.strip_suffix(".gigafile.nu").is_some_and(|prefix| {
        !prefix.is_empty() && prefix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

async fn build_file_plan(path: &Path, chunk_size: u64) -> Result<FilePlan, GfileError> {
    let file = File::open(path)
        .await
        .map_err(|source| io_error(source, path, IoOp::Read))?;
    let metadata = file
        .metadata()
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

    validate_chunk_count(metadata.len(), chunk_size)?;
    let chunks = chunk_plans(metadata.len(), chunk_size);
    let file = file.into_std().await;
    Ok(FilePlan {
        path: path.to_owned(),
        file_name,
        size: metadata.len(),
        chunks,
        source: Arc::new(file),
        fingerprint: source_fingerprint(&metadata),
    })
}

fn source_fingerprint(metadata: &std::fs::Metadata) -> SourceFingerprint {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;

    SourceFingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    }
}

async fn validate_source_unchanged(file_plan: &FilePlan) -> Result<(), GfileError> {
    let handle_metadata = file_plan
        .source
        .metadata()
        .map_err(|error| io_error(error, &file_plan.path, IoOp::Metadata))?;
    let path_metadata = fs::metadata(&file_plan.path)
        .await
        .map_err(|error| io_error(error, &file_plan.path, IoOp::Metadata))?;
    if source_fingerprint(&handle_metadata) != file_plan.fingerprint
        || source_fingerprint(&path_metadata) != file_plan.fingerprint
    {
        return Err(GfileError::UploadRejected {
            detail: "the local source file changed during upload; the upload was stopped to avoid mixing file versions"
                .to_owned(),
            status: None,
            retryable: false,
        });
    }
    Ok(())
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

fn validate_chunk_count(size: u64, chunk_size: u64) -> Result<(), GfileError> {
    let count = size.div_ceil(chunk_size);
    if count <= MAX_UPLOAD_CHUNKS && usize::try_from(count).is_ok() {
        Ok(())
    } else {
        Err(usage(format!(
            "upload would require {count} chunks; increase --chunk-size (maximum {MAX_UPLOAD_CHUNKS} chunks)"
        )))
    }
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
        let source = Arc::clone(&file_plan.source);
        pending.insert(
            chunk.index,
            tokio::task::spawn_blocking(move || read_prepared_chunk(source, path, chunk)),
        );
        *next_chunk += 1;
    }
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

fn read_prepared_chunk(
    source: Arc<std::fs::File>,
    path: PathBuf,
    chunk: ChunkPlan,
) -> Result<PreparedChunk, GfileError> {
    let len = usize::try_from(chunk.len)
        .map_err(|_| usage("upload chunk size is too large for this platform"))?;
    let mut body = vec![0_u8; len];
    read_exact_at(&source, &mut body, chunk.offset)
        .map_err(|source| io_error(source, &path, IoOp::Read))?;

    Ok(PreparedChunk {
        plan: chunk,
        body: Bytes::from(body),
    })
}

#[cfg(unix)]
fn read_exact_at(file: &std::fs::File, mut body: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt as _;

    while !body.is_empty() {
        match file.read_at(body, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                offset += read as u64;
                body = &mut body[read..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &std::fs::File, mut body: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt as _;

    while !body.is_empty() {
        match file.seek_read(body, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                offset += read as u64;
                body = &mut body[read..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
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
            Err(attempt_error)
                if !attempt_error.body_started
                    && upload_retryable(&attempt_error.error)
                    && attempt < context.options.retries =>
            {
                context.progress.set_position(confirmed_bytes);
                warn!(
                    "retrying upload chunk {} after error: {}",
                    chunk.index,
                    attempt_error.error.user_message()
                );
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            Err(attempt_error) => {
                context.progress.set_position(confirmed_bytes);
                return Err(attempt_error.error);
            }
        }
    }
}

async fn send_chunk_once(
    context: &ChunkUploadContext<'_>,
    chunk: ChunkPlan,
    confirmed_bytes: u64,
) -> Result<Value, ChunkAttemptError> {
    let activity = Arc::new(UploadActivity::new());
    let sent_in_attempt = Arc::new(AtomicU64::new(0));
    let activity_for_stream = Arc::clone(&activity);
    let sent_for_stream = Arc::clone(&sent_in_attempt);
    let progress_for_stream = context.progress.clone();
    let file = context
        .file_plan
        .source
        .try_clone()
        .map_err(|source| ChunkAttemptError {
            error: io_error(source, &context.file_plan.path, IoOp::Read),
            body_started: false,
        })?;
    let mut file = File::from_std(file);
    file.seek(SeekFrom::Start(chunk.offset))
        .await
        .map_err(|source| ChunkAttemptError {
            error: io_error(source, &context.file_plan.path, IoOp::Read),
            body_started: false,
        })?;
    let reader = file.take(chunk.len);
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
    let result = async {
        let response = send_with_idle_timeout(
            request,
            activity,
            context.options.timeout,
            "uploading chunk",
        )
        .await?;
        parse_upload_chunk_response(response, context.options.timeout).await
    }
    .await;
    result.map_err(|error| ChunkAttemptError {
        error,
        body_started: sent_in_attempt.load(Ordering::Relaxed) != 0,
    })
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
            Err(attempt_error)
                if !attempt_error.body_started
                    && upload_retryable(&attempt_error.error)
                    && attempt < context.options.retries =>
            {
                context.progress.set_segment_position(progress_index, 0);
                warn!(
                    "retrying upload chunk {} after error: {}",
                    prepared.plan.index,
                    attempt_error.error.user_message()
                );
                tokio::time::sleep(http::retry_delay(attempt)).await;
                attempt += 1;
            }
            Err(attempt_error) => {
                context.progress.set_segment_position(progress_index, 0);
                return Err(attempt_error.error);
            }
        }
    }
}

async fn send_prepared_chunk_once(
    context: &PreparedChunkUploadContext<'_>,
    prepared: &PreparedChunk,
) -> Result<Value, ChunkAttemptError> {
    let activity = Arc::new(UploadActivity::new());
    let sent_in_attempt = Arc::new(AtomicU64::new(0));
    let progress_index = progress_index(prepared.plan);
    let activity_for_stream = Arc::clone(&activity);
    let sent_for_stream = Arc::clone(&sent_in_attempt);
    let progress_for_stream = context.progress.clone();
    let body_for_stream = prepared.body.clone();
    let stream = futures_util::stream::unfold((body_for_stream, 0_usize), move |(body, offset)| {
        let activity_for_stream = Arc::clone(&activity_for_stream);
        let sent_for_stream = Arc::clone(&sent_for_stream);
        let progress_for_stream = progress_for_stream.clone();
        async move {
            if offset >= body.len() {
                return None;
            }
            let end = (offset + STREAM_CHUNK_SIZE).min(body.len());
            let bytes = body.slice(offset..end);
            activity_for_stream.mark();
            let sent = sent_for_stream.fetch_add(bytes.len() as u64, Ordering::Relaxed)
                + bytes.len() as u64;
            progress_for_stream.set_segment_position(progress_index, sent);
            Some((Ok::<Bytes, io::Error>(bytes), (body, end)))
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
    let result = async {
        let response = send_with_idle_timeout(
            request,
            activity,
            context.options.timeout,
            "uploading chunk",
        )
        .await?;
        parse_upload_chunk_response(response, context.options.timeout).await
    }
    .await;
    result.map_err(|error| ChunkAttemptError {
        error,
        body_started: sent_in_attempt.load(Ordering::Relaxed) != 0,
    })
}

async fn parse_upload_chunk_response(
    response: reqwest::Response,
    timeout: Duration,
) -> Result<Value, GfileError> {
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
        let retryable = http::is_retryable_status(response.status());
        return Err(GfileError::UploadRejected {
            detail: format!(
                "server returned HTTP {} for an upload chunk; re-upload the whole file",
                status
            ),
            status: Some(status),
            retryable,
        });
    }

    let body = http::read_body_limited(
        response,
        http::API_BODY_LIMIT,
        timeout,
        "reading upload response body",
    )
    .await?;
    serde_json::from_slice::<Value>(&body).map_err(|source| GfileError::UploadRejected {
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
    let page_bytes = match http::read_body_limited(
        page,
        http::PAGE_BODY_LIMIT,
        options.timeout,
        "reading upload verification page body",
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            warn!(
                "upload verification skipped because the download page body could not be read: {}",
                error.user_message()
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
    match content_length_via_range_get(client, &download_url, options).await {
        VerifyProbe::Length(actual) => compare_verified_size(expected, actual).map(Some),
        VerifyProbe::Unavailable => {
            warn!(
                "upload verification skipped because the download endpoint did not return a valid one-byte range"
            );
            Ok(None)
        }
    }
}

enum VerifyProbe {
    Length(u64),
    Unavailable,
}

async fn content_length_via_range_get(
    client: &reqwest::Client,
    url: &str,
    options: &UploadOptions,
) -> VerifyProbe {
    match send_verify_request(client, url, options).await {
        Ok(response) if response.status() == StatusCode::PARTIAL_CONTENT => {
            strict_range_total(response.headers())
                .map(VerifyProbe::Length)
                .unwrap_or(VerifyProbe::Unavailable)
        }
        Ok(_) | Err(_) => VerifyProbe::Unavailable,
    }
}

fn strict_range_total(headers: &header::HeaderMap) -> Option<u64> {
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/html"))
    {
        return None;
    }
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value != "1")
    {
        return None;
    }

    let value = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let range = value.strip_prefix("bytes ")?;
    let (span, total) = range.split_once('/')?;
    if span != "0-0" {
        return None;
    }
    total.parse::<u64>().ok().filter(|total| *total > 0)
}

async fn send_verify_request(
    client: &reqwest::Client,
    url: &str,
    options: &UploadOptions,
) -> Result<reqwest::Response, GfileError> {
    let mut attempt = 0;
    loop {
        let request = client
            .get(url)
            .header(header::RANGE, "bytes=0-0")
            .header(header::ACCEPT_ENCODING, "identity")
            .send();
        let result = tokio::time::timeout(options.timeout, request)
            .await
            .map_err(|_| timeout_network_error("starting upload verification request"))
            .and_then(|result| {
                result
                    .map_err(|source| network_error(source, "starting upload verification request"))
            });
        match result {
            Ok(response)
                if http::is_retryable_status(response.status()) && attempt < options.retries =>
            {
                drop(response);
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

fn observe_upload_response(
    chunk: ChunkPlan,
    response: &Value,
    state: &mut UploadResponseState,
) -> Result<(), GfileError> {
    debug!(
        chunk = chunk.index,
        response = %redact_upload_response(response),
        "upload chunk response"
    );
    match response.get("status").and_then(Value::as_i64) {
        Some(0) => {}
        Some(status) => {
            return Err(GfileError::UploadRejected {
                detail: format!("upload chunk {} returned status {status}", chunk.index),
                status: u16::try_from(status).ok(),
                retryable: false,
            });
        }
        None => {
            return Err(GfileError::UploadRejected {
                detail: format!(
                    "upload chunk {} response did not contain a numeric status field",
                    chunk.index
                ),
                status: None,
                retryable: false,
            });
        }
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
    Ok(())
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

fn timeout_network_error(context: &str) -> GfileError {
    GfileError::Network {
        source: boxed(io::Error::new(io::ErrorKind::TimedOut, "request timed out")),
        context: context.to_owned(),
    }
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
    fn read_ahead_window_accounts_for_the_active_chunk() {
        assert_eq!(bounded_read_ahead_window(16, 128 * 1024 * 1024), 4);
        assert_eq!(bounded_read_ahead_window(16, 300 * 1024 * 1024), 1);
        assert_eq!(bounded_read_ahead_window(16, 1024 * 1024 * 1024), 0);
        assert_eq!(bounded_read_ahead_window(3, 1024), 3);
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
    fn chunk_count_is_bounded_before_allocating_the_plan() {
        assert!(validate_chunk_count(MAX_UPLOAD_CHUNKS * MIN_CHUNK_SIZE, MIN_CHUNK_SIZE).is_ok());
        assert!(
            validate_chunk_count((MAX_UPLOAD_CHUNKS + 1) * MIN_CHUNK_SIZE, MIN_CHUNK_SIZE).is_err()
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

    #[test]
    fn upload_response_requires_zero_numeric_status() {
        let chunk = ChunkPlan {
            index: 2,
            offset: 0,
            len: 1,
        };
        for response in [serde_json::json!({"status": 1}), serde_json::json!({})] {
            let error =
                observe_upload_response(chunk, &response, &mut UploadResponseState::default())
                    .unwrap_err();
            assert!(matches!(error, GfileError::UploadRejected { .. }));
        }
    }

    #[test]
    fn upload_endpoint_restricts_production_origin() {
        assert_eq!(
            upload_endpoint("99.gigafile.nu", false).unwrap(),
            "https://99.gigafile.nu/upload_chunk.php"
        );
        assert!(upload_endpoint("http://99.gigafile.nu", false).is_err());
        assert!(upload_endpoint("https://example.com", false).is_err());
        assert!(upload_endpoint("https://99.gigafile.nu.evil.test", false).is_err());
        assert_eq!(
            upload_endpoint("http://127.0.0.1:1234", true).unwrap(),
            "http://127.0.0.1:1234/upload_chunk.php"
        );
    }
}
