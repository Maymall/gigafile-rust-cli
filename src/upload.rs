// SPDX-License-Identifier: GPL-3.0-only

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::TryStreamExt;
use reqwest::{Method, StatusCode, header, multipart};
use serde_json::Value;
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
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
    progress::ByteProgress,
    urlinfo::parse_download_url,
};

pub const MIN_CHUNK_SIZE: u64 = 1024 * 1024;
pub const MAX_CHUNK_SIZE: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_CHUNK_SIZE: u64 = 100 * 1024 * 1024;

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

pub async fn upload(options: UploadOptions) -> Result<UploadReport, GfileError> {
    validate_lifetime(options.lifetime)?;
    validate_chunk_size(options.chunk_size)?;
    let file_plan = build_file_plan(&options.file, options.chunk_size).await?;
    let client = http::build_client(options.user_agent.as_deref())?;
    let endpoint = upload_endpoint(&fetch_upload_server(&client, &options).await?);
    let upload_id = Uuid::new_v4().simple().to_string();
    let progress = ByteProgress::new(Some(file_plan.size), options.quiet, &file_plan.file_name);
    let mut uploaded_url = None;
    let mut confirmed_bytes = 0;
    let chunk_context = ChunkUploadContext {
        client: &client,
        endpoint: &endpoint,
        file_plan: &file_plan,
        chunks: file_plan.chunks.len() as u64,
        upload_id: &upload_id,
        options: &options,
        progress: &progress,
    };

    for chunk in &file_plan.chunks {
        let response = send_chunk_with_retries(&chunk_context, *chunk, confirmed_bytes).await?;
        if let Some(status) = response.get("status") {
            debug!(?status, chunk = chunk.index, "upload chunk status field");
        }
        if chunk.index == file_plan.chunks.len() as u64 - 1 {
            uploaded_url = response
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if uploaded_url.is_none() {
                return Err(GfileError::UploadRejected {
                    detail: "final upload response did not contain a download URL; re-upload the whole file".to_owned(),
                    status: None,
                    retryable: false,
                });
            }
        }
        confirmed_bytes += chunk.len;
        progress.set_position(confirmed_bytes);
    }
    progress.finish();

    let url = uploaded_url.expect("final chunk URL checked above");
    let verified = if options.verify {
        verify_uploaded_file(&client, &url, file_plan.size, &options).await?
    } else {
        None
    };

    Ok(UploadReport {
        url,
        bytes: file_plan.size,
        lifetime: options.lifetime,
        verified,
    })
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
}
