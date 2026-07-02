// SPDX-License-Identifier: GPL-3.0-only

use std::path::Path;

use serde::Serialize;

use crate::{
    download::{DownloadFileRecord, DownloadReport},
    error::GfileError,
    parser::download::PageKind,
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

fn download_file_json(file: &DownloadFileRecord) -> DownloadFileJson<'_> {
    DownloadFileJson {
        name: &file.name,
        path: file.path.as_deref().map(path_string),
        bytes: file.bytes,
        resumed: file.resumed,
        error: file.error.clone(),
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

fn print_json(value: &impl Serialize) -> Result<(), GfileError> {
    let text = serde_json::to_string(value).map_err(|source| GfileError::Parse {
        what: format!("failed to serialize JSON output: {source}"),
        hint: "This is an internal output error; please report it.".to_owned(),
    })?;
    println!("{text}");
    Ok(())
}
