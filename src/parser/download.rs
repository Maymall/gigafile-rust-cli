// SPDX-License-Identifier: MIT

use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use tracing::debug;

use crate::{error::GfileError, parser::size::parse_display_size};

// gfile.py@4c45392 lines 255-258: single-file pages use URL path as file id,
// `.dl_size` text for display size, and `#dl` text for the web filename.
const FILE_NAME_SELECTOR: &str = "#dl";
const SIZE_SELECTOR: &str = ".dl_size";
// gfile.py@4c45392 lines 247-252: matomete pages use this container and item
// layout; the onclick regex extracts the concrete file id for each item.
const MATOMETE_SELECTOR: &str = "#contents_matomete";
const MATOMETE_ITEM_SELECTOR: &str = ".matomete_file";
const MATOMETE_NAME_SELECTOR: &str = ".matomete_file_info > span:nth-child(2)";
const MATOMETE_SIZE_SELECTOR: &str = ".matomete_file_info > span:nth-child(3)";
const MATOMETE_DOWNLOAD_BUTTON_SELECTOR: &str = ".download_panel_btn_dl";
const MATOMETE_ONCLICK_FILE_ID_RE: &str = r"download\(\d+, *'(.+?)'";
const MATOMETE_SIZE_RE: &str = r"（(.+?)）";
const KEY_INPUT_SELECTOR: &str = "#dlkey";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInfo {
    pub kind: PageKind,
    pub files: Vec<RemoteFile>,
    pub needs_key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Single,
    Matomete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageState {
    Ok,
    NotFoundOrExpired,
    NeedsKey,
    WrongKey,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub file_id: String,
    pub raw_name: String,
    pub display_size: Option<String>,
    pub approx_bytes: Option<u64>,
}

pub fn classify_page(html: &str, status: u16) -> PageState {
    if matches!(status, 404 | 410) {
        return PageState::NotFoundOrExpired;
    }

    let document = Html::parse_document(html);
    let lower = html.to_ascii_lowercase();
    let contains_japanese_password =
        html.contains("パスワード") || html.contains("ダウンロードキー");
    let contains_japanese_missing =
        html.contains("削除") || html.contains("期限") || html.contains("見つかりません");

    if lower.contains("wrong password")
        || lower.contains("incorrect password")
        || lower.contains("password is wrong")
        || html.contains("パスワードが違")
        || html.contains("キーが違")
        || html.contains("ダウンロードキーが異なります")
    {
        return PageState::WrongKey;
    }

    // Live 2026-07-03: normal uploaded pages include disabled `#dlkey`;
    // only an enabled key input means a key is required before download.
    if has_enabled_key_input(&document).unwrap_or(false) {
        return PageState::NeedsKey;
    }

    if has_selector(&document, FILE_NAME_SELECTOR).unwrap_or(false)
        || has_selector(&document, SIZE_SELECTOR).unwrap_or(false)
        || has_selector(&document, MATOMETE_SELECTOR).unwrap_or(false)
    {
        return PageState::Ok;
    }

    if lower.contains("password")
        || lower.contains("dlkey")
        || lower.contains("download key")
        || contains_japanese_password
    {
        return PageState::NeedsKey;
    }

    // Expired and dangerous-file pages have not been safely reproduced in live
    // testing yet; keep the existing conservative text markers as fallback.
    if lower.contains("not found")
        || lower.contains("expired")
        || lower.contains("blocked")
        || lower.contains("danger")
        || lower.contains("var server =")
        || lower.contains("contents_upload")
        || contains_japanese_missing
    {
        return PageState::NotFoundOrExpired;
    }

    PageState::Unknown
}

pub fn parse_download_page(html: &str, file_id: &str) -> Result<PageInfo, GfileError> {
    let document = Html::parse_document(html);

    if has_selector(&document, MATOMETE_SELECTOR)? {
        return parse_matomete_page(&document);
    }

    parse_single_file_page(html, file_id)
}

pub fn parse_single_file_page(html: &str, file_id: &str) -> Result<PageInfo, GfileError> {
    let document = Html::parse_document(html);

    if select_first_text(&document, MATOMETE_SELECTOR)?.is_some() {
        return Err(parse_error(
            "matomete pages are not implemented in M1",
            "This build only supports single-file pages; matomete support is scheduled for M2.",
        ));
    }

    let raw_name = select_first_text(&document, FILE_NAME_SELECTOR)?
        .ok_or_else(|| parse_error("missing #dl", parse_hint()))?;
    debug!(raw_name = ?raw_name, "parsed raw_name");

    let display_size = select_first_text(&document, SIZE_SELECTOR)?
        .ok_or_else(|| parse_error("missing .dl_size", parse_hint()))?;
    let approx_bytes = parse_display_size(&display_size);

    Ok(PageInfo {
        kind: PageKind::Single,
        files: vec![RemoteFile {
            file_id: file_id.to_owned(),
            raw_name,
            display_size: Some(display_size),
            approx_bytes,
        }],
        needs_key: false,
    })
}

fn parse_matomete_page(document: &Html) -> Result<PageInfo, GfileError> {
    let item_selector = parse_selector(MATOMETE_ITEM_SELECTOR)?;
    let items: Vec<_> = document.select(&item_selector).collect();
    if items.is_empty() {
        return Err(parse_error(
            "matomete container has no .matomete_file items",
            parse_hint(),
        ));
    }

    let onclick_re = Regex::new(MATOMETE_ONCLICK_FILE_ID_RE).expect("valid matomete onclick regex");
    let size_re = Regex::new(MATOMETE_SIZE_RE).expect("valid matomete size regex");
    let mut files = Vec::with_capacity(items.len());

    for item in items {
        let raw_name = select_first_text_in(&item, MATOMETE_NAME_SELECTOR)?.ok_or_else(|| {
            parse_error(
                "missing matomete filename selector .matomete_file_info > span:nth-child(2)",
                parse_hint(),
            )
        })?;
        debug!(raw_name = ?raw_name, "parsed matomete raw_name");

        let onclick = select_first_attr_in(&item, MATOMETE_DOWNLOAD_BUTTON_SELECTOR, "onclick")?
            .ok_or_else(|| {
                parse_error(
                    "missing matomete download onclick",
                    "Page structure may have changed; rerun with --dump-page and -vv.",
                )
            })?;
        let file_id = onclick_re
            .captures(&onclick)
            .and_then(|captures| captures.get(1))
            .map(|value| value.as_str().to_owned())
            .ok_or_else(|| {
                parse_error(
                    "matomete download onclick did not contain a file id",
                    parse_hint(),
                )
            })?;

        let size_text = select_first_text_in(&item, MATOMETE_SIZE_SELECTOR)?.ok_or_else(|| {
            parse_error(
                "missing matomete size selector .matomete_file_info > span:nth-child(3)",
                parse_hint(),
            )
        })?;
        let display_size = size_re
            .captures(&size_text)
            .and_then(|captures| captures.get(1))
            .map(|value| value.as_str().trim().to_owned())
            .ok_or_else(|| {
                parse_error("matomete size text did not contain （...）", parse_hint())
            })?;
        let approx_bytes = parse_display_size(&display_size);

        files.push(RemoteFile {
            file_id,
            raw_name,
            display_size: Some(display_size),
            approx_bytes,
        });
    }

    Ok(PageInfo {
        kind: PageKind::Matomete,
        files,
        needs_key: false,
    })
}

fn select_first_text(document: &Html, selector: &str) -> Result<Option<String>, GfileError> {
    let selector = parse_selector(selector)?;

    Ok(document
        .select(&selector)
        .next()
        .map(|node| node.text().collect::<String>().trim().to_owned()))
}

fn select_first_text_in(
    element: &ElementRef<'_>,
    selector: &str,
) -> Result<Option<String>, GfileError> {
    let selector = parse_selector(selector)?;
    Ok(element
        .select(&selector)
        .next()
        .map(|node| node.text().collect::<String>().trim().to_owned()))
}

fn select_first_attr_in(
    element: &ElementRef<'_>,
    selector: &str,
    attr: &str,
) -> Result<Option<String>, GfileError> {
    let selector = parse_selector(selector)?;
    Ok(element
        .select(&selector)
        .next()
        .and_then(|node| node.value().attr(attr))
        .map(ToOwned::to_owned))
}

fn has_selector(document: &Html, selector: &str) -> Result<bool, GfileError> {
    let selector = parse_selector(selector)?;
    Ok(document.select(&selector).next().is_some())
}

fn has_enabled_key_input(document: &Html) -> Result<bool, GfileError> {
    let selector = parse_selector(KEY_INPUT_SELECTOR)?;
    Ok(document
        .select(&selector)
        .any(|node| node.value().attr("disabled").is_none()))
}

fn parse_selector(selector: &str) -> Result<Selector, GfileError> {
    Selector::parse(selector).map_err(|_| {
        parse_error(
            format!("invalid selector {selector}"),
            "This is an internal parser bug; please report it.",
        )
    })
}

fn parse_error(what: impl Into<String>, hint: impl Into<String>) -> GfileError {
    GfileError::Parse {
        what: what.into(),
        hint: hint.into(),
    }
}

fn parse_hint() -> &'static str {
    "Page structure may have changed; rerun with --dump-page and -vv, then report the fixture."
}
