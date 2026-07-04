// SPDX-License-Identifier: MIT

use std::{path::PathBuf, time::Duration};

use reqwest::Url;
use tokio::fs;

use crate::{
    error::{GfileError, IoOp},
    http,
    parser::download::{PageKind, PageState, classify_page, parse_download_page},
    urlinfo::parse_download_url,
};

#[derive(Debug, Clone)]
pub struct InfoOptions {
    pub url: String,
    pub timeout: Duration,
    pub retries: u32,
    pub user_agent: Option<String>,
    pub dump_page: Option<PathBuf>,
    pub allow_any_host: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoReport {
    pub kind: PageKind,
    pub key_required: bool,
    pub files: Vec<InfoFileRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoFileRecord {
    pub index: usize,
    pub display_name: String,
    pub display_size: Option<String>,
    pub approx_bytes: Option<u64>,
}

pub async fn info(options: InfoOptions) -> Result<InfoReport, GfileError> {
    let url_info = parse_download_url(&options.url, options.allow_any_host)?;
    let client = http::build_client(options.user_agent.as_deref())?;
    let response = http::get_with_retries_and_timeout(
        &client,
        &url_info.page_url,
        options.retries,
        "fetching info page",
        Some(options.timeout),
    )
    .await?;
    let status = response.status().as_u16();
    let final_url = response.url().clone();
    let bytes = response
        .bytes()
        .await
        .map_err(|source| GfileError::Network {
            source: Box::new(source),
            context: "reading info page body".to_owned(),
        })?;

    if let Some(path) = &options.dump_page {
        fs::write(path, &bytes)
            .await
            .map_err(|source| GfileError::Io {
                source,
                path: path.to_owned(),
                op: IoOp::Write,
            })?;
        eprintln!("Warning: dumped page may contain private filenames; do not share it publicly.");
    }

    let html = String::from_utf8_lossy(&bytes);
    if redirected_to_gigafile_home(&final_url) {
        return Err(GfileError::NotFoundOrExpired);
    }

    let state = classify_page(&html, status);
    let key_required = match state {
        PageState::Ok => false,
        PageState::NeedsKey => true,
        PageState::WrongKey => return Err(GfileError::KeyWrong),
        PageState::NotFoundOrExpired => return Err(GfileError::NotFoundOrExpired),
        PageState::Unknown => {
            return Err(GfileError::Parse {
                what: "download page state is unknown".to_owned(),
                hint: "Page structure may have changed; rerun with --dump-page and -vv.".to_owned(),
            });
        }
    };

    report_from_html(&html, &url_info.file_id, key_required)
}

pub fn report_from_html(
    html: &str,
    file_id: &str,
    key_required: bool,
) -> Result<InfoReport, GfileError> {
    let page = parse_download_page(html, file_id)?;
    Ok(InfoReport {
        kind: page.kind,
        key_required,
        files: page
            .files
            .into_iter()
            .enumerate()
            .map(|(offset, file)| InfoFileRecord {
                index: offset + 1,
                display_name: file.raw_name,
                display_size: file.display_size,
                approx_bytes: file.approx_bytes,
            })
            .collect(),
    })
}

fn redirected_to_gigafile_home(url: &Url) -> bool {
    url.path() == "/"
        && url
            .host_str()
            .is_some_and(|host| host.ends_with("gigafile.nu"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_ID: &str = "0123abcd-000000example";

    #[test]
    fn info_reports_single_fixture() {
        let report = report_from_html(
            include_str!("../tests/fixtures/single_basic.html"),
            FILE_ID,
            false,
        )
        .unwrap();

        assert_eq!(report.kind, PageKind::Single);
        assert!(!report.key_required);
        assert_eq!(report.files[0].index, 1);
        assert_eq!(report.files[0].display_name, "example file.bin");
        assert_eq!(report.files[0].display_size.as_deref(), Some("10KB"));
    }

    #[test]
    fn info_reports_unicode_single_fixture() {
        let report = report_from_html(
            include_str!("../tests/fixtures/single_japanese.html"),
            FILE_ID,
            false,
        )
        .unwrap();

        assert_eq!(report.kind, PageKind::Single);
        assert_eq!(report.files[0].display_name, "テスト資料_2026.zip");
    }

    #[test]
    fn info_reports_matomete_fixture() {
        let report = report_from_html(
            include_str!("../tests/fixtures/matomete_two_files.html"),
            FILE_ID,
            false,
        )
        .unwrap();

        assert_eq!(report.kind, PageKind::Matomete);
        assert_eq!(report.files.len(), 2);
        assert_eq!(report.files[0].index, 1);
        assert_eq!(report.files[1].index, 2);
    }

    #[test]
    fn info_reports_unicode_matomete_fixture() {
        let report = report_from_html(
            include_str!("../tests/fixtures/matomete_unicode.html"),
            FILE_ID,
            false,
        )
        .unwrap();

        assert_eq!(report.kind, PageKind::Matomete);
        assert_eq!(report.files.len(), 2);
    }

    #[test]
    fn info_reports_key_required_fixture_without_error() {
        let report = report_from_html(
            include_str!("../tests/fixtures/page_needs_key.html"),
            FILE_ID,
            true,
        )
        .unwrap();

        assert_eq!(report.kind, PageKind::Single);
        assert!(report.key_required);
        assert_eq!(report.files[0].display_name, "example file.bin");
    }

    #[test]
    fn info_empty_matomete_fixture_is_parse_error() {
        let error = report_from_html(
            include_str!("../tests/fixtures/matomete_empty.html"),
            FILE_ID,
            false,
        )
        .unwrap_err();

        assert!(matches!(error, GfileError::Parse { .. }));
    }
}
