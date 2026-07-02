// SPDX-License-Identifier: GPL-3.0-only

use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    time::Duration,
};

use regex::Regex;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
};
use tracing::{info, warn};

use crate::{
    error::{BoxError, GfileError, IoOp},
    http,
    jsonout::{self, ErrorJson},
    naming::{log_name_diagnostics, sanitize_server_filename},
    parser::download::{
        PageInfo, PageKind, PageState, RemoteFile, classify_page, parse_download_page,
    },
    progress::ByteProgress,
    urlinfo::parse_download_url,
};

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub url: String,
    pub output: Option<PathBuf>,
    pub force: bool,
    pub no_resume: bool,
    pub key: Option<String>,
    pub timeout: Duration,
    pub retries: u32,
    pub user_agent: Option<String>,
    pub dump_page: Option<PathBuf>,
    pub quiet: bool,
    pub allow_any_host: bool,
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
    pub error: Option<ErrorJson>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SingleDownloadOutcome {
    name: Option<String>,
    path: PathBuf,
    bytes: u64,
    resumed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PartSidecar {
    version: u8,
    file_id: String,
    expected: Option<u64>,
    key_used: bool,
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
    total: Option<u64>,
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
    validate_output_for_page(&page, options.output.as_deref())?;

    let mut records = Vec::with_capacity(page.files.len());
    let mut first_error = None;
    for remote_file in &page.files {
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
    let mut target_path = final_path.to_owned();
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
    if resume.range_start.is_none() {
        if let (Some(dir), Some(name)) = (header_output_dir.as_deref(), header_name.as_deref()) {
            let header_path = dir.join(sanitize_server_filename(name, &remote_file.file_id));
            if header_path != target_path {
                ensure_target_available(&header_path, options.force)?;
                target_path = header_path;
                let (part_path, sidecar_path) = part_paths(&target_path)?;
                resume.part_path = part_path;
                resume.sidecar_path = sidecar_path;
            }
        }
    }

    let transfer = transfer_plan(&response, &resume)?;
    if transfer.expected_total.is_none() {
        warn!("download response has no Content-Length; exact size check is disabled");
    }
    warn_on_display_size_mismatch(remote_file, transfer.expected_total);

    let mut response = response;
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
        &remote_file.raw_name,
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
    })
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
        warn!("existing .part has missing or damaged sidecar; restarting from zero");
        return Ok(ResumePlan {
            part_path,
            sidecar_path,
            range_start: None,
            expected: None,
        });
    };

    if sidecar.version != 1 || sidecar.file_id != remote_file.file_id || sidecar.expected.is_none()
    {
        warn!("existing .part sidecar does not match this file; restarting from zero");
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
    let total = if &captures[3] == "*" {
        None
    } else {
        Some(captures[3].parse::<u64>().map_err(|_| GfileError::Parse {
            what: format!("invalid Content-Range total in {value:?}"),
            hint: "Retry with --no-resume; if it repeats, report the response headers.".to_owned(),
        })?)
    };
    Ok(ContentRange { start, total })
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
