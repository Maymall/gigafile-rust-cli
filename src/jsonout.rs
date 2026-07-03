// SPDX-License-Identifier: GPL-3.0-only

use std::path::Path;

use serde::Serialize;

use crate::{
    download::{DownloadFileRecord, DownloadReport},
    error::GfileError,
    info::{InfoFileRecord, InfoReport},
    parser::download::PageKind,
    upload::UploadReport,
};

#[derive(Debug, Serialize)]
struct DownloadReportJson<'a> {
    status: &'static str,
    kind: &'static str,
    files: Vec<DownloadFileJson<'a>>,
    failed: usize,
}

#[derive(Debug, Serialize)]
struct DownloadFileJson<'a> {
    name: &'a str,
    path: Option<String>,
    bytes: Option<u64>,
    resumed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    threads: Option<u8>,
    error: Option<ErrorJson>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct ErrorJson {
    pub code: &'static str,
    pub exit_code: u8,
    pub message: String,
}

pub fn print_download_report(report: &DownloadReport) -> Result<(), GfileError> {
    let json = DownloadReportJson {
        status: "ok",
        kind: kind_name(report.kind),
        files: report.files.iter().map(download_file_json).collect(),
        failed: report.failed,
    };
    print_json(&json)
}

pub fn print_upload_report(report: &UploadReport) -> Result<(), GfileError> {
    let json = UploadReportJson {
        status: "ok",
        url: &report.url,
        delkey: report.delkey.as_deref(),
        remote_filename: report.remote_filename.as_deref(),
        expires_at_estimate: report.expires_at_estimate.as_deref(),
        bytes: report.bytes,
        lifetime: report.lifetime,
        verified: report.verified,
    };
    print_json(&json)
}

pub fn print_info_report(report: &InfoReport) -> Result<(), GfileError> {
    let json = InfoReportJson {
        status: "ok",
        kind: kind_name(report.kind),
        key_required: report.key_required,
        files: report.files.iter().map(info_file_json).collect(),
    };
    print_json(&json)
}

pub fn print_error(error: &GfileError) -> Result<(), GfileError> {
    let json = ErrorJson {
        code: error.code(),
        exit_code: error.exit_code(),
        message: error.user_message(),
    };
    print_json(&ErrorEnvelope {
        status: "error",
        code: json.code,
        exit_code: json.exit_code,
        message: json.message,
    })
}

pub fn error_json(error: &GfileError) -> ErrorJson {
    ErrorJson {
        code: error.code(),
        exit_code: error.exit_code(),
        message: error.user_message(),
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    status: &'static str,
    code: &'static str,
    exit_code: u8,
    message: String,
}

#[derive(Debug, Serialize)]
struct UploadReportJson<'a> {
    status: &'static str,
    url: &'a str,
    delkey: Option<&'a str>,
    remote_filename: Option<&'a str>,
    expires_at_estimate: Option<&'a str>,
    bytes: u64,
    lifetime: u16,
    verified: Option<bool>,
}

#[derive(Debug, Serialize)]
struct InfoReportJson<'a> {
    status: &'static str,
    kind: &'static str,
    key_required: bool,
    files: Vec<InfoFileJson<'a>>,
}

#[derive(Debug, Serialize)]
struct InfoFileJson<'a> {
    display_name: &'a str,
    display_name_may_be_masked: bool,
    display_size: Option<&'a str>,
    approx_bytes: Option<u64>,
}

fn download_file_json(file: &DownloadFileRecord) -> DownloadFileJson<'_> {
    DownloadFileJson {
        name: &file.name,
        path: file.path.as_deref().map(path_string),
        bytes: file.bytes,
        resumed: file.resumed,
        threads: file.threads,
        error: file.error.clone(),
    }
}

fn info_file_json(file: &InfoFileRecord) -> InfoFileJson<'_> {
    InfoFileJson {
        display_name: &file.display_name,
        display_name_may_be_masked: true,
        display_size: file.display_size.as_deref(),
        approx_bytes: file.approx_bytes,
    }
}

fn path_string(path: &Path) -> String {
    path.display().to_string()
}

fn kind_name(kind: PageKind) -> &'static str {
    match kind {
        PageKind::Single => "single",
        PageKind::Matomete => "matomete",
    }
}

pub fn print_json(value: &impl Serialize) -> Result<(), GfileError> {
    let text = serde_json::to_string(value).map_err(|source| GfileError::Parse {
        what: format!("failed to serialize JSON output: {source}"),
        hint: "This is an internal output error; please report it.".to_owned(),
    })?;
    println!("{text}");
    Ok(())
}
